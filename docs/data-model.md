# Data Model

## Project identity

Projects are allocated for the exact root selected by the caller, normalized lexically with `path.resolve` (never `realpath`). New project IDs are opaque `prj_<16 hex>` values. Existing remote-shaped or path-shaped IDs are retained as readable compatibility records and are never rekeyed. An active exact root is idempotent; archived-only matches do not resurrect an old project. Workspace `projectId` is stable membership: reconciliation may update git-derived kind and branch metadata, but never rehomes a workspace or changes a project's root, ID, or default name.

`projectKey` is a persisted, opaque equivalence key used only to group the same logical project
across hosts. It is separate from the host-local `projectId`; today's producer prefers a normalized
Git remote and otherwise uses the local project root. Consumers never derive it from live Git.
Creation persists it with the project, and normal boot reconciliation fills it for
older records where the field is absent—there is no migration.

`kind` and `projectKey` are mutable metadata, not identity. Workspace reconciliation watches active project roots and
updates those fields and `updatedAt` when Git facts change, preserving the project's ID, root path,
names, and workspace foreign keys. Attached workspaces are independently refreshed
from their own cwd, so an explicit project root never implies a workspace checkout. Empty projects
are observed too.

The workspace registry model defines placement once: initial directory/worktree construction,
mutable reconciliation fields, and the persisted-to-wire checkout projection. Its update policy
preserves `displayName` and `baseBranch`. `WorkspaceProvisioningService` owns the corresponding
registry writes, so directory opens, agent imports, and worktree creation all enter through that
service instead of constructing records independently. The workspace record is then the durable
placement authority: `cwd` is the exact execution directory, while `worktreeRoot` is the backing
checkout root. They intentionally differ for an exact subproject inside a worktree. Archive,
restore, branch auto-name, and descriptor flows consume those persisted facts rather than
rediscovering ownership from a directory that may already be gone. Reconciliation may refresh
mutable placement facts, but never changes `projectId`, `cwd`, `displayName`, or `baseBranch`.
Workspace archive runs lifecycle teardown from the exact `cwd` but removes only the backing
`worktreeRoot` after its last active reference disappears. Worktree recovery recreates that backing
checkout from `mainRepoRoot`, then restores the relative path from `worktreeRoot` to `cwd`.

Paseo uses **file-based JSON persistence**. The Rust daemon owns validation and mutation in `packages/daemon/src/config`, `agent`, `projects`, and `automation`. Existing persisted records, IDs, provider handles and configuration shapes remain compatible; the TypeScript-style shapes below describe data, not a Node runtime. Store implementations own atomic writes and compatibility normalization. Do not reset a home or rekey existing records as part of the native client cutover.

All server-side stores live under `$PASEO_HOME` (defaults to `~/.paseo`).

## Store Surface Rules

Store APIs own persistence atomicity and should not make services coordinate raw reads and writes. A good store method maps cleanly to one SQL statement or one SQL transaction, even when the current implementation is JSON files. If a caller needs a queue, lock, read-merge-write loop, or uniqueness race workaround, that behavior belongs behind the store surface.

---

## Directory layout

```
$PASEO_HOME/
├── config.json                          # Daemon configuration
├── server-id                            # Stable daemon identifier (plain text, "srv_<base64url>")
├── daemon-keypair.json                  # E2EE keypair for relay (mode 0600)
├── paseo.pid                            # Daemon PID lock file
├── local-credential                     # Per-run local client credential (mode 0600)
├── daemon.log                           # Default log file (path configurable)
├── agents/
│   └── {sanitized-cwd}/
│       └── {agentId}.json               # One file per agent
├── schedules/
│   └── {scheduleId}.json                # One file per schedule
├── projects/
│   ├── projects.json                    # Project registry
│   ├── workspaces.json                  # Workspace registry
│   ├── workspace-labels.json            # Shared host-local label catalog
│   ├── workspace-labels.transaction.json # Recoverable catalog/assignment compound commit
│   └── icons/                           # Host-local custom project icon images
├── runtime/
│   └── managed-processes/
│       └── {recordId}.json              # Helper processes owned by Paseo; reconciled on daemon bootstrap
├── plugins/
│   ├── sources.json                      # Managed kind and Git acquisition remote
│   └── {pluginId}/{uuid}/                # Managed native Git/Cargo source or release archive
└── push-tokens.json                     # Expo push subscription leases
```

The `agents/{sanitized-cwd}/` directory name is derived from the agent's `cwd` by stripping the filesystem root and replacing path separators with `-` (Windows drive letters become a `C-` style prefix). Persistent server stores write atomically by writing a temp file in the target directory and then renaming it into place.

---

## 1. Agent Record

**Path:** `$PASEO_HOME/agents/{project-dir}/{agentId}.json`

Each agent is stored as a separate JSON file, grouped by project directory.

| Field                | Type                                     | Description                                                                                                                                                                                                                                                                                                                                                                         |
| -------------------- | ---------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `id`                 | `string`                                 | UUID, primary key                                                                                                                                                                                                                                                                                                                                                                   |
| `provider`           | `string`                                 | Agent provider (`"claude"`, `"codex"`, `"opencode"`, etc.)                                                                                                                                                                                                                                                                                                                          |
| `cwd`                | `string`                                 | Working directory the agent operates in                                                                                                                                                                                                                                                                                                                                             |
| `workspaceId`        | `string?`                                | Owning workspace id — the single source of ownership. Every agent is stamped with one at create time. Runtime code never infers ownership or status from cwd: status is computed per `workspaceId`, and same-cwd siblings are independent.                                                                                                                                         |
| `createdAt`          | `string` (ISO 8601)                      | Creation timestamp                                                                                                                                                                                                                                                                                                                                                                  |
| `updatedAt`          | `string` (ISO 8601)                      | Last update timestamp                                                                                                                                                                                                                                                                                                                                                               |
| `lastActivityAt`     | `string?` (ISO 8601)                     | Last activity timestamp                                                                                                                                                                                                                                                                                                                                                             |
| `lastUserMessageAt`  | `string?` (ISO 8601)                     | Last user message timestamp                                                                                                                                                                                                                                                                                                                                                         |
| `title`              | `string?`                                | User-visible title                                                                                                                                                                                                                                                                                                                                                                  |
| `labels`             | `Record<string, string>`                 | Key-value labels (default `{}`). Paseo uses `paseo.parent-agent-id` for parentage and client-scoped `paseo.open-agent-tab.*` labels while managed subagent tabs are open — see [agent-lifecycle.md](./agent-lifecycle.md)                                                                                                                                                           |
| `lastStatus`         | `AgentStatus`                            | One of: `"initializing"`, `"idle"`, `"running"`, `"error"`, `"closed"`. `closed` means the record is resumable but has no live provider runtime; archive remains represented separately by `archivedAt`.                                                                                                                                                                            |
| `lastModeId`         | `string?`                                | Last active mode ID                                                                                                                                                                                                                                                                                                                                                                 |
| `config`             | `SerializableConfig?`                    | Agent session configuration (see below)                                                                                                                                                                                                                                                                                                                                             |
| `runtimeInfo`        | `RuntimeInfo?`                           | Live runtime state (see below)                                                                                                                                                                                                                                                                                                                                                      |
| `features`           | `AgentFeature[]?`                        | Provider-reported features (toggles/selects)                                                                                                                                                                                                                                                                                                                                        |
| `persistence`        | `PersistenceHandle?`                     | Handle for resuming sessions                                                                                                                                                                                                                                                                                                                                                        |
| `lastError`          | `string?` (nullable)                     | Last error message, if any                                                                                                                                                                                                                                                                                                                                                          |
| `requiresAttention`  | `boolean?`                               | Whether the agent needs user attention                                                                                                                                                                                                                                                                                                                                              |
| `attentionReason`    | `"finished" \| "error" \| "permission"?` | Why attention is needed                                                                                                                                                                                                                                                                                                                                                             |
| `attentionTimestamp` | `string?` (ISO 8601)                     | When attention was flagged                                                                                                                                                                                                                                                                                                                                                          |
| `internal`           | `boolean?`                               | Whether this is a system-internal agent                                                                                                                                                                                                                                                                                                                                             |
| `archivedAt`         | `string?` (ISO 8601)                     | Soft-delete timestamp                                                                                                                                                                                                                                                                                                                                                               |

### Nested: SerializableConfig

| Field              | Type                       | Description                  |
| ------------------ | -------------------------- | ---------------------------- |
| `title`            | `string?`                  | Configured title             |
| `modeId`           | `string?`                  | Configured mode              |
| `model`            | `string?`                  | Configured model             |
| `thinkingOptionId` | `string?`                  | Thinking/reasoning level     |
| `featureValues`    | `Record<string, unknown>?` | Feature preference overrides |
| `extra`            | `Record<string, any>?`     | Provider-specific config     |
| `systemPrompt`     | `string?`                  | Custom system prompt         |
| `mcpServers`       | `Record<string, any>?`     | MCP server configurations    |

### Nested: RuntimeInfo

| Field              | Type                       | Description                    |
| ------------------ | -------------------------- | ------------------------------ |
| `provider`         | `string`                   | Active provider                |
| `sessionId`        | `string?`                  | Active session ID              |
| `model`            | `string?`                  | Active model                   |
| `thinkingOptionId` | `string?`                  | Active thinking option         |
| `modeId`           | `string?`                  | Active mode                    |
| `extra`            | `Record<string, unknown>?` | Provider-specific runtime data |

### Nested: PersistenceHandle

| Field          | Type                   | Description                                                           |
| -------------- | ---------------------- | --------------------------------------------------------------------- |
| `provider`     | `string`               | Provider that owns the session                                        |
| `sessionId`    | `string`               | Session ID for resumption                                             |
| `nativeHandle` | `any?`                 | Provider-specific handle (Codex thread ID, Claude resume token, etc.) |
| `metadata`     | `Record<string, any>?` | Extra metadata                                                        |

### Nested: AgentFeature (discriminated union on `type`)

**Toggle:**

| Field         | Type       |
| ------------- | ---------- |
| `type`        | `"toggle"` |
| `id`          | `string`   |
| `label`       | `string`   |
| `description` | `string?`  |
| `tooltip`     | `string?`  |
| `icon`        | `string?`  |
| `value`       | `boolean`  |

**Select:**

| Field         | Type                  |
| ------------- | --------------------- |
| `type`        | `"select"`            |
| `id`          | `string`              |
| `label`       | `string`              |
| `description` | `string?`             |
| `tooltip`     | `string?`             |
| `icon`        | `string?`             |
| `value`       | `string \| null`      |
| `options`     | `AgentSelectOption[]` |

---

## Runtime-only Terminal Sessions

Terminals are live daemon state, not persisted JSON records. A terminal carries a `workspaceId` while it is running; workspace-scoped terminal lists include only terminals with the matching `workspaceId`. Legacy live terminals without an owner remain visible to unscoped terminal reads but contribute to no workspace status.

Terminal activity contributes to the workspace status bucket **per `workspaceId`**: a working terminal drives `running` onto the workspace it carries only. Same-`cwd` siblings are untouched; terminal visibility is likewise `workspaceId`-scoped.

---

## 2. Daemon Configuration

**Path:** `$PASEO_HOME/config.json`

Single file, validated with `PersistedConfigSchema`.

`agents.skills.selection` is the daemon host's orchestration-skill preference. Missing means
`{ mode: "all" }`. Installed state is not persisted; the daemon derives it from the shared
`~/.agents/skills` and Claude skill directories and keeps config plus filesystem convergence behind
one serialized owner. Codex discovers the shared directory. Updates retire unchanged files from
Paseo-managed copies in `~/.codex/skills`, preserving user edits and untracked files.

`paseo reload` reads and validates this file once inside the daemon. That snapshot drives resolution,
classification, application, and reload bookkeeping. `DaemonConfigStore` owns applying runtime-safe
fields and their removal/default semantics; session handlers and the CLI only relay the structured
result. Normal config patches persist only the requested fields, so launch overrides and resolved
defaults never leak into the file. Startup-only fields remain compared with the daemon's launch
snapshot so a mixed edit can apply its live subset and still name the paths that require restart.

```
{
  version: 1,
  daemon: {
    listen: "127.0.0.1:6767",
    hostnames: true | string[],   // legacy alias `allowedHosts` is migrated on load
    trustedProxies: true | string[], // defaults to ["loopback"]; Express proxy names/CIDRs
    mcp: { enabled: boolean, injectIntoAgents: boolean },
    git: { maxProcessesPerSecond: number, maxProcessConcurrency: number },
    appendSystemPrompt: string,    // appended to supported provider system/developer prompts
    terminalProfiles: TerminalProfile[],  // named shell commands; omitted means DEFAULT_TERMINAL_PROFILES
    agentProfiles: AgentProfile[],        // named agent launch bundles; omitted means none
    cors: { allowedOrigins: string[] },
    relay: { enabled: boolean, endpoint: string, publicEndpoint: string, useTls: boolean, publicUseTls: boolean }, // new homes materialize enabled: false
    auth: { password: string }    // bcrypt hash, optional
  },
  app: {
    baseUrl: string
  },
  worktrees?: {
    root?: string            // optional root for new worktrees; defaults to $PASEO_HOME/worktrees
    servicePorts?: {         // optional dynamic service port allocation policy
      range?: string         // inclusive range, e.g. "3000-4000"
      portScript?: string    // executable that receives service/workspace context and prints one TCP port
    }
  },
  agents: {
    skills?: {
      selection?: { mode: "all" } | { mode: "custom", skills: string[] }
    },
    // Legacy provider command shapes are normalized by the Rust configuration owner.
    // Custom provider IDs must declare `extends` (one of the built-ins or `"acp"`)
    // and `label`. See `packages/daemon/src/config`.
    providers: Record<providerId, ProviderOverride>,
    metadataGeneration: {
      providers: [{ provider, model?, thinkingOptionId? }]
    }
  },
  pluginsEnabled: boolean,
  plugins: Record<pluginId, { source: "directory", path: string, enabled?: boolean }>,
  log: {
    level, format,
    console: { level, format },
    file: { level, path, rotate: { maxSize, maxFiles } }
  }
}
```

All fields are optional with sensible defaults.

Managed plugins appear as directory sources in `config.json`; it owns the active path and enabled
state. `plugins/sources.json` is written atomically and stores only managed kind and the Git acquisition
remote. Installed revision, package/subdirectory identity and ownership root come from retained
artifacts and the fixed managed layout; [native source acquisition](../public-docs/plugins/publishing.md)
explains their authority. An update prepares a new directory before replacing the active path and
removing the previous version. Old record fields are accepted at the store boundary and ignored;
there is no startup migration or persistent update policy.

### Profile lists

`terminalProfiles` and `agentProfiles` are both whole-list fields: a config patch replaces the
array, never merges entries, so a client sends the complete next list on every add, edit, reorder
and remove. List order is the display order.

Absent and empty mean different things for terminal profiles — omitting the key falls back to
`DEFAULT_TERMINAL_PROFILES`, while `[]` means the user removed them all. Agent profiles have no
defaults, so both mean none.

`PersistedConfigSchema` parses strictly, so a daemon that predates a field drops it on write
rather than storing something it cannot describe. That is why the client gates the agent profiles
UI on `server_info.features.agentProfiles` instead of letting a save appear to succeed against an
older daemon.

### Agent provider Paseo tools

`agents.providers` is keyed by the exact provider ID used to launch the agent. The built-in IDs are
`claude`, `codex`, `copilot`, `opencode`, `pi`, and `omp`. Custom provider IDs are their literal
configuration keys, such as `my-claude` or `zai`, not the provider named by `extends`.

Each entry may include a Paseo-tool policy:

```json
{
  "agents": {
    "providers": {
      "my-claude": {
        "extends": "claude",
        "label": "My Claude",
        "paseoTools": {
          "enabled": true,
          "disabledTools": ["browser_evaluate"]
        }
      }
    }
  }
}
```

Absent `paseoTools`, or absent fields within it, means Paseo tools are enabled and all tools are
allowed. `enabled: false` disables the provider's Paseo catalog; `disabledTools` lists exact tool
IDs to omit. The policy covers the core and browser catalog, not the voice-only `speak` tool.
Browser tools also require `daemon.browserTools.enabled` and a connected browser host.
This policy controls the catalog presented to an agent. It is not an authorization boundary for
agents that can access the host through a shell.

`daemon.mcp.injectIntoAgents` is the global override. When it is `false`, no provider receives
Paseo tools; otherwise the provider policy applies. Provider and global policy are resolved when a
session is created, resumed, imported, or reloaded, so configuration changes affect the next
session rather than an already-running one.

`agents.metadataGeneration.providers` selects the models for daemon-side metadata tasks: commit messages, PR text, branch names, and generated titles. An empty list is Automatic; a non-empty list is Manual and only those models are tried. See [public-docs/metadata-generation.md](../public-docs/metadata-generation.md#how-a-model-is-chosen).

### Git process limits

Git process limits are global to one daemon. The start-rate limit defaults to `64` processes per
second, and the concurrency limit defaults to `8`:

```json
{
  "daemon": {
    "git": {
      "maxProcessesPerSecond": 64,
      "maxProcessConcurrency": 8
    }
  }
}
```

`maxProcessesPerSecond` limits Git process starts in any one-second interval. The allowance can
start as a burst; it does not wait for earlier processes to exit. `maxProcessConcurrency` limits
the number of Git processes that have started but not exited. Every Git command uses both limits,
including initial workspace reads, filesystem-triggered refreshes, background checks, and explicit
requests.

Environment variables override `config.json`:

| Environment variable                 | Setting                  |
| ------------------------------------ | ------------------------ |
| `PASEO_GIT_MAX_PROCESSES_PER_SECOND` | `maxProcessesPerSecond`  |
| `PASEO_GIT_MAX_PROCESS_CONCURRENCY`  | `maxProcessConcurrency`  |
| `PASEO_GIT_CONCURRENCY`              | Legacy concurrency alias |

`PASEO_GIT_MAX_PROCESS_CONCURRENCY` wins when it and the legacy alias are both set. Run `paseo reload`
after changing `config.json`. Environment changes require a daemon restart; the launch environment
remains authoritative during reload.

### Removed speech settings

Legacy top-level `providers` speech settings and `features.dictation`/`features.voiceMode` are discarded during configuration normalization. Agent provider settings under `agents.providers` remain independent and are preserved.

---

## 3. Schedule

**Path:** `$PASEO_HOME/schedules/{id}.json`

One file per schedule. ID is 8 hex characters.

Manual runs belong to the daemon once admitted, not to the requesting socket. A disconnected
caller must not strand a durable `running` claim or skip its workspace cleanup. Cancellation is
a failed run for both existing-agent and new-agent targets; an earlier assistant reply is not a
successful result for the canceled work.

| Field       | Type                                  | Description                      |
| ----------- | ------------------------------------- | -------------------------------- |
| `id`        | `string`                              | 8-char hex ID                    |
| `name`      | `string?`                             | Human-readable name              |
| `prompt`    | `string`                              | The prompt to send               |
| `cadence`   | `ScheduleCadence`                     | Timing (see below)               |
| `target`    | `ScheduleTarget`                      | What to run (see below)          |
| `status`    | `"active" \| "paused" \| "completed"` | Current state                    |
| `createdAt` | `string` (ISO 8601)                   |                                  |
| `updatedAt` | `string` (ISO 8601)                   |                                  |
| `nextRunAt` | `string?` (ISO 8601)                  | Next scheduled execution         |
| `lastRunAt` | `string?` (ISO 8601)                  | Last execution time              |
| `pausedAt`  | `string?` (ISO 8601)                  | When paused                      |
| `expiresAt` | `string?` (ISO 8601)                  | Auto-expire time                 |
| `maxRuns`   | `number?`                             | Max executions before completing |
| `runs`      | `ScheduleRun[]`                       | Execution history                |

### Nested: ScheduleCadence (discriminated union on `type`)

- `{ type: "cron", expression: string, timezone?: string }` — canonical cadence for new writes; absent `timezone` means UTC
- `{ type: "every", everyMs: number }` — legacy rolling interval, still readable and executable during the compatibility window

### Nested: ScheduleTarget (discriminated union on `type`)

- `{ type: "agent", agentId: string }` — send to existing agent
- `{ type: "new-agent", config: { provider, cwd, modeId?, model?, thinkingOptionId?, title?, providerOptions?, featureValues?, systemPrompt?, mcpServers? } }` — create a new agent

### Nested: ScheduleRun

| Field          | Type                                   | Description             |
| -------------- | -------------------------------------- | ----------------------- |
| `id`           | `string`                               | Run ID                  |
| `scheduledFor` | `string` (ISO 8601)                    | Intended execution time |
| `startedAt`    | `string` (ISO 8601)                    |                         |
| `endedAt`      | `string?` (ISO 8601)                   |                         |
| `status`       | `"running" \| "succeeded" \| "failed"` |                         |
| `agentId`      | `string?` (UUID)                       | Agent used for this run |
| `output`       | `string?`                              | Agent output text       |
| `error`        | `string?`                              | Error message if failed |

---

## 4. Project Registry

**Path:** `$PASEO_HOME/projects/projects.json`

Array of project records.

| Field                | Type                        | Description                                                                                                                                |
| -------------------- | --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `projectId`          | `string`                    | Host-local primary key; new records use opaque `prj_<16 hex>` IDs                                                                          |
| `projectKey`         | `string \| null`            | Persisted opaque cross-host grouping key; reconciliation backfills absent values                                                           |
| `rootPath`           | `string`                    | Exact lexically normalized selected root; never realpathed                                                                                 |
| `kind`               | `"git" \| "non_git"`        | Mutable Git observation about `rootPath`, never a membership key                                                                           |
| `displayName`        | `string`                    | Selected-root basename, stable across remote and Git changes                                                                               |
| `customName`         | `string \| null`            | User-set override layered over `displayName`. Null means "use the derived name".                                                           |
| `customIconRevision` | `string \| null`            | Identifies the host-local custom icon stored under `projects/icons/`. Null means the icon is discovered by scanning the project directory. |
| `createdAt`          | `string` (ISO 8601)         |                                                                                                                                            |
| `updatedAt`          | `string` (ISO 8601)         |                                                                                                                                            |
| `archivedAt`         | `string \| null` (ISO 8601) | Soft-delete timestamp; required nullable                                                                                                   |

Uploading a file and pasting a website or image URL are two ways of _acquiring_ the same custom
icon. The client fetches URL imports and sends their bytes through the upload RPC. The daemon never
receives or fetches the URL; it validates the uploaded bytes, stores them, and records a new
`customIconRevision`. Going back to automatic deletes the stored image, as does removing the
project.

Active exact roots are idempotent using lexical platform-equivalence semantics. Existing legacy
remote-shaped and path-shaped IDs remain readable, including duplicate roots; reconciliation never
merges them, transfers names, archives them, or moves workspace foreign keys. An explicit
workspace `projectId` is authoritative when it names an active project, regardless of cwd
containment. Archived-only exact-root records are not resurrected by explicit add/open; a fresh
opaque project is allocated instead. Agent restore is separate and restores the agent's existing
workspace together with its owning project.

---

## 5. Workspace Registry

**Path:** `$PASEO_HOME/projects/workspaces.json`

Array of workspace records. A workspace is a specific working directory within a project.

| Field                          | Type                                                         | Description                                                                                                                                                                                   |
| ------------------------------ | ------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `workspaceId`                  | `string`                                                     | Opaque stable identifier (`wks_<hex>`), generated independently of the directory. MUST NOT be treated as a path; compare by exact equality. Use the `cwd` field for directory access.         |
| `projectId`                    | `string`                                                     | FK to Project.projectId; the workspace's stable project membership                                                                                                                            |
| `cwd`                          | `string`                                                     | Exact execution directory selected for agents, files, scripts, and setup                                                                                                                      |
| `kind`                         | `"local_checkout" \| "worktree" \| "directory"`              | Mutable checkout classification                                                                                                                                                               |
| `displayName`                  | `string`                                                     | The human name (the generated/derived title). Decoupled from `branch` by construction.                                                                                                        |
| `title`                        | `string \| null`                                             | User-set name override layered over `displayName`. Null means "use `displayName`".                                                                                                            |
| `branch`                       | `string \| null`                                             | The current Git branch for git-backed workspaces. Separate from `displayName`/`title`; a background branch refresh never rewrites the name.                                                   |
| `worktreeRoot`                 | `string \| null`                                             | Backing checkout/worktree root. May differ from `cwd` for exact subprojects and remains persisted after the worktree is deleted so restore can reproduce the placement.                       |
| `baseBranch`                   | `string \| null`                                             | Comparison base retained across archive and restore. Branch-off creation stores the resolved ref; legacy and PR-checkout records hold a bare name. Null means no recorded base.               |
| `isPaseoOwnedWorktree`         | `boolean`                                                    | Whether Paseo owns and may remove/recreate the backing `worktreeRoot`                                                                                                                         |
| `mainRepoRoot`                 | `string \| null`                                             | Main repository root for worktree checkouts, independent of both exact `cwd` and backing `worktreeRoot`                                                                                       |
| `createdAt`                    | `string` (ISO 8601)                                          |                                                                                                                                                                                               |
| `updatedAt`                    | `string` (ISO 8601)                                          |                                                                                                                                                                                               |
| `archivedAt`                   | `string \| null` (ISO 8601)                                  | Soft-delete; required nullable                                                                                                                                                                |
| `autoArchivedChangeRequestUrl` | `string \| null`                                             | Change request whose merged state triggered auto-archive. Restore replaces it with the current merged change request, when present, so repeated snapshots cannot archive the workspace again. |
| `labels`                       | `string[]?`                                                  | Normalized display names assigned from this host's shared label catalog. Missing means unlabelled.                                                                                            |
| `pinnedAt`                     | `string \| null` (ISO 8601)                                  | Pinned-to-top-of-sidebar timestamp; null means "not pinned"                                                                                                                                   |
| `untrustedSource`              | `{ kind: "change_request", forge, number, headRepository }?` | Provenance captured when a cross-repository change request creates the workspace. Missing means repository automation is allowed; explicit setup removes the field.                           |

> **Opaque-ID invariant:** `workspaceId` is opaque identity, never a filesystem path. Filesystem and git operations take `cwd`/`workspaceDirectory` only — never the id. The daemon never materializes registries or workspace ownership from agent records at startup.

### Workspace label catalog

**Path:** `$PASEO_HOME/projects/workspace-labels.json`

The catalog is shared by every workspace on one host. A definition contains a display name and one
of the ten identity colour names in the shared wire contract; trimmed, collapsed, case-insensitive name identity is
unique within that file. Definitions have no portable ID or principal owner and remain after their
last assignment. Editing a label takes a new name, a new colour, or both in one commit, so the two
fields cannot land half-applied. Workspaces store label names, so a rename and a delete rewrite
workspace assignments through one serialized compound commit while a recolour is catalog-only; a
rename onto a name the host already has is refused rather than merged. A prepared
`workspace-labels.transaction.json` contains both before and after images. The daemon writes the
catalog and workspace files, then atomically changes the transaction to committed; that phase
change is the durable commit point. Recovery rolls prepared transactions back before either
directory is served. A committed marker proves both data files were already written, so recovery
only loads the current catalog and retries marker cleanup; it never reapplies stale workspace
after-images over later registry mutations. A request rejected before the commit point therefore
cannot take effect after restart. If the live daemon cannot determine or restore the durable state,
the workspace registry freezes every write and later label mutations fail with
`workspace_label_storage_uncertain` until daemon restart performs recovery. Reads remain available,
but may reflect the last acknowledged cache until restart. Workspace directory and catalog updates
publish only after the commit point; publication failure does not roll durable state back.

`projectId` is still a real FK: workspace records should have a matching project record. Read-only
history surfaces tolerate transient orphaned workspaces by omitting those rows so one bad FK cannot
blank the whole History screen, but mutation paths should repair or remove the orphaned state rather
than treating it as valid.

---

## 6. Push Token Store

**Path:** `$PASEO_HOME/push-tokens.json`

```json
{
  "subscriptions": [{ "token": "ExponentPushToken[...]", "expiresAt": "2026-11-03T00:00:00.000Z" }]
}
```

Expo push subscription leases. A live client heartbeat renews its token's `expiresAt`; expired leases receive no notifications. Entries without a token or an RFC 3339 `expiresAt` are skipped, and a repeated token keeps its last expiry. Written atomically with mode `0600`.

---

## 7. Daemon meta files

These small files are persisted under `$PASEO_HOME` for native daemon identity and runtime coordination.

| Path                  | Format                                                         | Notes                                                                             |
| --------------------- | -------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| `server-id`           | Plain text, e.g. `srv_<base64url>`                             | Stable per-`$PASEO_HOME` daemon ID. Overridable via `PASEO_SERVER_ID` env.        |
| `daemon-keypair.json` | `{ v: 2, publicKeyB64, secretKeyB64 }` (libsodium box keypair) | E2EE relay identity. Written with mode `0600`. Regenerated if file is unreadable. |
| `paseo.pid`           | JSON `{ pid, startedAt, ... }`                                 | PID lock; prevents two daemons sharing one `$PASEO_HOME`.                         |
| `local-credential`    | 32 random bytes encoded as base64url text                      | Rotated before each listen and deleted on shutdown; mode `0600`.                  |
| `daemon.log`          | Native daemon log output                                       | Default location; selected-home logging configuration applies.                    |

---

## Client-side stores

The native client keeps its state in the platform application-support directory, independently of the daemon's `$PASEO_HOME`. The Rust client uses the same directory and files as the Flutter client it replaces, so existing installs keep their hosts, preferences and layout. `packages/gui-core/src/storage` owns this boundary; `paseo-gui-platform` resolves the directory, and `PASEO_GUI_STATE_DIR` replaces it for development runs (`cargo xtask dev app` sets it to `.dev/gui-state`).

Under `paseo-native/`:

- `state.json` contains versioned preferences, saved hosts, route suppression and layout. Unknown keys are retained. A corrupt file or a newer unsupported version is never overwritten.
- `state.lock` serializes updates. Writes merge changes three-way against the latest file so another window's or process's unrelated preference and host edits are not lost.
- `layout-<window>.json` holds a window's saved tab and pane layout. Restoration drops removed browser and executable plugin tabs.
- `replica-<base64url-server-id>.json` stores a host's project/workspace/agent snapshot and cache timestamp. It is scoped to the saved server identity and reconciled with the live daemon.

Next to it, `shared_preferences.json` keeps keyed values (client id, relay protection floors `paseo.relayProtected.<key>`, update-operation records) in the Flutter Linux/Windows format, including the `flutter.` key prefix. Foreign keys survive writes. On macOS and iOS (NSUserDefaults binary plist) and Android (`FlutterSharedPreferences.xml`) the Flutter values are imported once into that file.

Composer drafts, queues and creation intents live in its `paseo.chat.drafts.v1` value, keyed by `[serverId, workspaceId, targetId]`, not in the general `preferences` object. The composer owns this data; retained image bytes live in immutable content-addressed blob files rather than repeated inline draft copies. A late acknowledgement may clear only the submitted editor generation, never newer input.

Core state writes use atomic replacement; Linux/macOS restrict private state files to mode 0600 and directories to 0700. Windows profile ACLs and mobile sandboxes provide the platform boundary. Core commands run in order, and send/creation workflows wait for flushed draft state before submitting dependent requests.

Directory identity includes `(serverId, cwd)`; equal paths on two hosts must not merge. Workspace-owned state remains scoped to workspace identity, not parsed from a pathname. Closing a tab changes layout, not the daemon's archive state.

The client does not read or import the retired app's browser IndexedDB, React Native AsyncStorage, Electron localStorage or desktop settings. Cached client state is not the authoritative session history; the daemon remains the authority. Treat cached prompts, paths, credentials and tool output as sensitive data and do not infer encryption at rest from encrypted transport.
