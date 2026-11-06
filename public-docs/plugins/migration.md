---
title: Migrate a plugin to the native backend runtime
description: Port backend behavior to Rust executables without a JavaScript or UI compatibility fallback.
nav: Migration
order: 47
category: Plugins
---

# Migrate a plugin to the native backend runtime

This is the **0.13.1 source migration**, not a publication announcement. Existing JavaScript/TypeScript plugins are **not executable in the new runtime**. The daemon rejects legacy runtime manifests with an actionable native migration error while preserving installation records and settings. It does not load a Node worker, install an interpreter, or silently delete/enable old plugins. `PASEO_PLUGIN_NODE` and `PASEO_PLUGIN_WORKER` are not migration mechanisms.

The migration is a real port, not renaming an entry or changing a version requirement. Existing Rust built-ins remain native; Kiro remains disabled and is not part of these instructions. External agent CLIs are separate products, not Paseo's plugin runtime.

## 1. Classify the plugin

| Old behavior | Native implementation |
| --- | --- |
| RPC/vendor calls | `Plugin::rpc` with explicit JSON validation and real Rust I/O. |
| Lifecycle events/transforms | `Plugin::on`/`before`, cancellation and owned-resource cleanup. |
| Settings schemas/migrations | `SettingsDefinition`, `Plugin::settings`, compatible stored envelopes and revisions. |
| Direct provider | `Provider`/`Connection`, real session events, permissions and cleanup. |
| ACP adapter | `paseo_plugin::acp::AcpProvider` with a command or owned stream; port vendor extensions to `AcpTransformer`. This replaces `runAcpProvider` behavior, not its JavaScript source API. |
| Usage discovery/fetch | `UsageSource`, explicit input validation and account/credential boundaries. |
| Screens, panels, sidebar/header components, attachment pickers, themes | No executable client plugin equivalent. Remove obsolete UI registrations. |
| React timeline transforms/renderers | Emit standard protocol items; do not require extension code to read important output. |
| Client-only plugin | Not a supported native plugin; do not claim that an empty native backend is a migration. |

Native Markdown/source views and configuration descriptors are product features/data contracts, not arbitrary extension UI slots. There is no browser/webview fallback for old client code.

## 2. Build a native executable

Use `paseo plugin init <new-empty-directory> --sdk-path <checkout>/packages/plugin` as a starting point, then port the actual behavior. The project contains `Cargo.toml`, `src/main.rs`, `paseo-plugin.json` and author docs. Use Rust 1.93+, edition 2024, and the real `paseo-plugin` crate; do not add a dependency on an unpublished registry version.

```json
{
  "id": "review-tools",
  "runtime": "native",
  "requirements": { "paseo": ">=0.13.1" },
  "server": { "executable": "target/release/paseo-plugin-server", "args": [] },
  "build": [["cargo", "build", "--release", "--target-dir", "target"]]
}
```

Match the executable to your Cargo binary target and use `.exe` on Windows. It must remain inside the plugin directory and be an actual native binary, not a script that invokes the old entry. Commands are argv arrays, not shell fragments. `Plugin::run` supplies `--validate` and normal initialize/ready IPC. See the [reference](reference.md#validation-and-readiness).

Port I/O and dependencies to Rust, then remove obsolete runtime entries, package-manager manifests, compiler configuration and UI-only modules from the **new distributed plugin**. Do not delete the user's old installation or stored settings as part of a source port. Pin an available SDK Git revision or use the local path while developing.

## 3. Port SDK calls explicitly

| Former concept | Native API/source |
| --- | --- |
| Server contribution entry | Construct `Plugin`, register before `run().await`. |
| `handle`/`defineRpc` schemas | `Plugin::rpc`; validate `Value` input/output with Serde/domain checks. No automatic Zod equivalent. |
| `on`/`before` callbacks | Async `(Value, Context) -> Result<Value>`; before input is `{request: ...}`, null preserves it. |
| Returned cleanup callback | `Plugin::cleanup`; observe `Context::cancellation`. |
| `defineSettings`/`registerSettings` | `SettingsDefinition::new(...).migrate(...)`, `Plugin::settings`. |
| Server-side Paseo client | `context.daemon.client().await?` gives `Arc<paseo_client::DaemonClient>`. |
| Provider registration | `paseo_plugin::provider::{Provider, Connection, Events, Metadata}`. |
| Usage registration | `UsageSource::new`, `Plugin::usage_source`; `paseo_plugin::usage` helpers/types. |
| JavaScript process helpers | Real Rust process ownership and platform-specific cleanup, not compatibility shims. |

Author SDK: [packages/plugin](../../packages/plugin/src/lib.rs); host: [daemon plugins](../../packages/daemon/src/plugins.rs); client: [Rust SDK reference](../sdk/reference.md). No old SDK export implies UI support.

## 4. Preserve data and behavior

Keep installation IDs, RPC names, payload meaning and settings IDs stable where compatibility is intended. Settings keep host-local `{version, values}` envelopes, opaque revisions, defaults, migration validation, conflict detection and invalid-data retention. Test migration against actual old stored data without overwriting it on parse errors. Settings APIs do not create native settings screens.

Before hooks return the **whole transformed request** or null. Preserve prior transforms and unrelated fields. `agent.session_open` changes only launch environment overrides; do not use it to bypass daemon policy. Hook event payloads/cancellation and cleanup matter as much as successful registration.

Providers must preserve capability negotiation, prompt receipts, permission decisions, cancellation, timeline identity, optional persistence/replay and child-session restoration. Never claim unsupported optional behavior. Standard user/assistant text, reasoning, tools, todos, errors, notifications and compaction render without custom plugin code. See [provider guidance](providers.md).

Usage callbacks must discover only the scoped account, avoid secrets in identifiers/input/logs, and not refresh or rewrite another CLI's credentials. The daemon-client bridge replaces ad hoc duplicate RPCs for ordinary host operations. Access to that bridge is trusted daemon access, not a security sandbox.

## 5. Change acquisition and build requirements

Use directory, Git, Cargo or checksum-pinned HTTPS release sources. Old npm sources cannot execute or update through npm. **Do not use remove/reinstall to preserve settings:** removal deletes the installation's settings directory, and duplicate-ID installation is rejected. There is currently no atomic same-ID managed-source replacement command.

For an explicit non-destructive **directory runtime migration**:

1. Disable the old installation with `paseo plugin disable <id>`.
2. Separately obtain/build a trusted native port and run its `--validate <directory> <daemon-version>`.
3. Back up the selected daemon configuration/settings. Edit only the existing `plugins[<id>]` configuration entry to `{"source":"directory","path":"/absolute/path/to/native-plugin","enabled":false}`, preserving the installation ID and every other configuration entry.
4. Apply the configuration with `paseo reload --json`, then deliberately enable/reload the same ID and inspect status/logs.

This keeps `plugin-settings/<id>` and old managed receipts/files. It does **not** migrate the old managed source record; source-management commands can continue to report the legacy-source migration error. A managed receipt/source replacement workflow is a separate missing capability, not something to hide by deleting records. Use a test copy first and keep backups before migrating real data.

Git/Cargo source plugins need the host Rust toolchain and declared build dependencies. Prebuilt plugins need only compatible host runtime dependencies. Build the repository daemon with `cargo xtask build-daemon`; no Node build step is required. Use a separate test daemon/home, not the user's production daemon. Remote installation paths exist on the daemon host and are not uploaded by the CLI.

Distribute the manifest, actual native executable or reproducible source/build inputs, assets and notices. Pin dependencies, commit Cargo locks for `--locked` source builds, and document supported OS/architecture. See [distribution](publishing.md). A broad `requirements.paseo` range cannot convert an old plugin into a native one.

## 6. Exercise the migrated behavior

For a scaffold-shaped project, on an isolated test host:

```sh
cargo build --release --target-dir target
./target/release/paseo-plugin-server --validate . 0.13.1
paseo plugin install /absolute/path/to/native-plugin
paseo plugin ls <installed-id>
paseo plugin logs <installed-id>
```

Use the actual executable name and `.exe` on Windows. After edits, rebuild and `paseo plugin reload <installed-id>`. Both root `pluginsEnabled` and installation enablement must allow execution. Exercise every retained RPC, hook, setting/migration, usage source and real provider turn/permission path, including cleanup while active. `running` and successful validation are not behavioral coverage. No publication, deployment or test execution is claimed here.

The repository's maintained runnable example is [workspace-inspector](index.md#run-the-repository-example), not the old JavaScript example directories. Former client-only examples cannot be made useful by adding a no-op backend; state the unsupported feature plainly.
