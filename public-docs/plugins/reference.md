---
title: Plugin reference
description: Native executable manifests, Rust SDK contracts, IPC and source management.
nav: Reference
order: 48
category: Plugins
---

# Plugin reference

See the [quickstart](index.md) and [migration guide](migration.md). This documents the **0.13.1 source checkout**, not publication or deployment.

## Runtime boundary

The daemon and built-ins run in Rust. Third-party plugins are native executables launched by the daemon, communicating through bounded JSONL stdin/stdout; there is no dynamic-library ABI, JavaScript worker or interpreter fallback. The native client renders supported protocol data, not executable plugin UI. Plugins, validation and build commands are unsandboxed daemon-user code.

Sources of truth: [SDK](../../packages/plugin/src/lib.rs), [settings](../../packages/plugin/src/settings.rs), [provider traits](../../packages/plugin/src/provider.rs), [daemon bridge](../../packages/plugin/src/daemon.rs), [host manifest](../../packages/daemon/src/plugins/manifest.rs) and [CLI](../../packages/daemon/src/cli/catalog/plugins.rs). Use local path or explicitly pinned Git dependencies while the SDK is unpublished.

## Project files

`paseo plugin init <directory> --sdk-path <checkout>/packages/plugin` creates a Rust executable project, manifest and author documentation. It requires an empty directory, does not build it, and writes an absolute SDK path dependency. Without `--sdk-path`, it uses a Git dependency. Compile the executable before activation.

### Requirements and runtime entries

```json
{
  "id": "review-tools",
  "name": "Review tools",
  "description": "Reviews local changes",
  "runtime": "native",
  "requirements": { "paseo": ">=0.13.1" },
  "server": { "executable": "target/release/paseo-plugin-server", "args": [] },
  "build": [["cargo", "build", "--release", "--target-dir", "target"]]
}
```

Use `.exe` on Windows. The plugin ID starts with a lowercase letter and contains lowercase letters, digits or hyphens. `server.args` is required, even when empty; arguments are passed directly, without shell concatenation. The executable must be a normalized relative path to a regular native ELF/Mach-O/PE file inside the plugin directory, including after symlink resolution. Absolute paths, traversal, scripts and interpreter shims are rejected; Unix files need executable permission. A valid header does not prove host architecture compatibility.

Declare the Paseo semver range you actually support. Omitted `requirements.paseo` means the historical `<0.8.0` default, not unrestricted compatibility. Empty/invalid ranges and unknown requirement keys fail. A version change alone cannot port an old plugin.

### Validation and readiness

The host appends `--validate <absolute-plugin-directory> <daemon-version>` to `server.args`. The executable must exit successfully and print one JSON object, at most 1 MiB:

```json
{"valid":true,"plugin":{"id":"review-tools"},"methods":[],"hooks":{"events":[],"before":[]},"providers":[],"usageSources":[]}
```

`Plugin::run()` handles this mode, reading the manifest and returning registered metadata without invoking handlers. Registration code and executable startup still run: validation is **not** a safe way to inspect untrusted code.

Normal startup receives `initialize` with `pluginId`, `pluginDirectory`, `appVersion` and, for settings, `settingsDirectory`. It emits the same registration fields with `type: "ready"` instead of `valid`/`plugin`. Register all contributions before `run()`. RPC/hook/usage requests return matching `requestId` results or errors; provider connections have separate acceptance and event messages. Frames are bounded to 16 MiB and output is backpressured. Reserve stdout exclusively for IPC; use `Context::log(...).await` or stderr for diagnostics.

## Entry point and cleanup

This complete `src/main.rs` uses the scaffold's dependencies:

```rust
use paseo_plugin::{Plugin, Result, ensure};
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> Result<()> {
    let mut plugin = Plugin::new();
    plugin.rpc("greeting.create", |input, _context| async move {
        let name = input["name"].as_str().unwrap_or("");
        ensure!(!name.is_empty(), "name must be a nonempty string");
        Ok(json!({"message": format!("Hello, {name}!")}))
    })?;
    plugin.on("agent.turn_ended", |event, context| async move {
        context.log(format!("Turn ended: {} ({})", event["agent"]["id"], event["outcome"])).await?;
        Ok(Value::Null)
    })?;
    plugin.run().await
}
```

Callbacks receive `serde_json::Value` and `Context`, and return an async `Result<Value>`. **Validate input and output yourself**, using Serde/domain checks or an explicit schema validator. `Plugin::rpc` validates registration names and duplicates, not arbitrary application payload schemas. Registration does not create a native button or screen; consumers use plugin RPC through the daemon protocol.

Use `Plugin::cleanup` for resources you own. Its callback receives null and a context; return `Ok(Value::Null)` after joining tasks and closing sockets/processes/subscriptions. `Context::cancellation` signals request cancellation or shutdown. Reload, disable, removal and daemon shutdown end the process and registrations. Cleanup has a bounded deadline; do not depend on it running after a crash or forced termination. Hooks operate without a connected app.

## Lifecycle hooks

| Registration | Input and result |
| --- | --- |
| `plugin.on(name, callback)` | Event JSON; return null after handling. Handlers run in registration order; an error is logged without skipping later handlers. |
| `plugin.before(name, callback)` | `{"request": ...}`; return null to preserve the request, or the complete transformed request. |

Events: `agent.created`, `agent.closed`, `agent.turn_started`, `agent.turn_ended`, `agent.permission_requested`, `agent.permission_resolved`, `agent.archived`, `workspace.created`, `workspace.archived`. Public background agents participate; private disposable helpers do not. Archive notification does not prove cleanup has finished. Turn-end payloads include `agent`, `turnId` and `outcome` (`completed`, `failed`, `canceled`); do not rely on the deprecated empty `timeline` field.

Before hooks: `agent.create` can transform public configuration except `cwd`, plus launch `env`; `agent.session_open` can change only `env`; `workspace.create` transforms an explicit workspace creation request. Preserve unrelated fields and previous transforms. `env` contains launch overrides, not the whole inherited environment. Session opening includes create/resume/refresh/import and history work. Respect cancellation; do not assume every permission is a shell command or every permission resolution is a completed turn.

## Connected hosts and the Paseo SDK {#connected-hosts}

Call `context.daemon.client().await?` to obtain an `Arc<paseo_client::DaemonClient>`. This lazy connection uses the plugin IPC bridge, including text/binary frames, not an extra token or network connection. Host closure invalidates the cached session; a later call opens another logical session. Use the [Rust client SDK](../sdk/reference.md) for ordinary agent, workspace, permission, provider and configuration operations. Fetch bounded history when necessary rather than logging private conversation contents by default. Host IDs, paths and workspace IDs are scoped to their daemon connection, not globally interchangeable.

## Persisted settings {#settings-screens}

Register `SettingsDefinition::new(id, positive_version, validator)` with `plugin.settings(...)`. The validator accepts JSON and returns normalized JSON with defaults; `{}` must produce the default document. Optional `.migrate(|values, from_version| async move { ... })` converts older stored data before validation. See the complete [inspection settings implementation](../../plugin-examples/workspace-inspector/src/lib.rs).

The returned cloneable `Settings` exposes `read().await`, `write(revision, values).await`, `reset(revision).await` and `subscribe()` (a Tokio broadcast receiver). IDs match `[a-z][a-z0-9_-]*`. Storage is host-scoped and preserves the existing `{version, values}` envelope. Schema version differs from opaque write revision.

- Read returns `status: "ready"`, `values`, `revision`, or `status: "invalid"`, `error`, `revision`. Invalid saved data is retained until explicitly repaired/reset.
- Write/reset requires the read revision; stale writes return `conflict`. Successful writes return `saved`; invalid values return `invalid`. Failed writes do not notify subscribers.
- Subscriptions deliver states; a lag error means re-read to synchronize. Subscriber failure does not undo a saved write.
- RPCs `settings.<id>.read`, `.write`, `.reset` are registered automatically, with `settings.changed` notification on changes.

These are data APIs, not executable settings screens.

## Providers

Implement `paseo_plugin::provider::{Provider, Connection}` and register with `plugin.provider(...)`. `Provider::connect` receives request JSON, `Events` and `Context`; return `Arc<dyn Connection>`. Negotiate only offered versions/capabilities. `Connection::send` reports acceptance, not turn completion. Publish typed-by-protocol JSON through `Events::emit`; implement `close` to stop tasks/processes and resolve pending permissions. Optional status/catalog-cache capabilities must agree with metadata. Full session, permission, configuration, persistence and timeline contracts are covered in the [provider guide](providers.md).

`Metadata::icon_path` is a relative SVG file path contained in the plugin directory, not inline SVG or a URL. `UsageSource::icon` is **inline SVG text**, a different wire field. Host validation limits SVG to 64 KiB and rejects active content such as scripts, foreign objects, styles, event handlers and external references. Listing PNGs have a separate [distribution contract](publishing.md#icons-and-screenshots).

For ACP agents, register [`paseo_plugin::acp::AcpProvider`](../../packages/plugin/src/acp/mod.rs) instead of rebuilding the provider protocol yourself. `AcpProvider::command(id, label, argv)` and `::connector(id, label, factory)` return `Result<AcpProvider>`; builders `.options(AcpOptions)`, `.transformer(impl AcpTransformer)`, `.description(...)` and `.icon_path(...)` return the provider. The connector returns a fresh owned `AcpStream` asynchronously and receives a cancellation token. `AcpTransformer` exposes ordered catalog discovery/configuration and notification/tool-snapshot transforms; `AcpContext::config` provides `ConfigAccess::read`/`set`. See the [ACP guide](providers.md#adapt-an-acp-agent) for lifecycle and capability boundaries. This is a native implementation of the former ACP helper functionality, not JavaScript source compatibility.

## Usage sources

`UsageSource::new(id, label, discover, fetch)` registers with `plugin.usage_source(...)`. Both async callbacks receive JSON plus `Context`. Validate scope/input before accessing files or contacting a service; there is no automatic Zod/schema parser.

The public [`paseo_plugin::usage` module](../../packages/plugin/src/usage.rs) supplies Serde `Scope`, `Account`, `Report`, `Problem`, `Window`, `Balance`, `Detail`, `Unit` and `Tone` types. Deserialize callback input with `serde_json::from_value` and serialize typed output with `serde_json::to_value`. Helpers include `hash_account_key`, `used_pct_of`, `tone_from_used_pct`, `balance_tone_from_remaining`, `window_from_used_pct` and `window_from_reported_duration`. The duration helper takes reported seconds, optional stable scope ID/label, explicit unknown ID/label/short-label, used percentage and reset time; never pass a reset countdown as the period duration.

- Discovery receives global or session scope and returns accounts with stable `key`, optional `label`/`harness`, and JSON `input` identifying a credential store, **never credentials**. Session scope includes `provider`, optional `model` and launch `env`; return only the session's account, or `[]` for unrelated/missing logins. Do not scan other agents' stores as fallback.
- Fetch returns `available` with windows and optional balances/details, `unavailable` with a typed problem, or `error` with a message. Re-read credentials; do not redeem refresh tokens or rewrite another CLI's store.
- Account keys use 1–128 characters from `[A-Za-z0-9._-]`. Hash sensitive identities; choose identity stable across token rotation, not email/token contents.
- Unavailable problems distinguish `expired` (`expiresAt`, optional `refreshedBy`), `rejected` (`status`, optional `refreshedBy`) and `no_quota` (`detail`). `refreshedBy` names a CLI, not a user-facing instruction.
- Derive window names from reported duration or provider names, not response order or reset countdown. Keep window IDs stable across resets/reordering.

Built-in usage integrations already run in Rust; no extra executable or custom native panel is required.

## Plugin sources

| Source | Selector |
| --- | --- |
| Host directory | `/srv/paseo/plugins/review` or `./review` |
| GitHub/Git | `github:acme/paseo-review`, `git:acme/paseo-review`, `git:https://git.example/acme/review.git` |
| Cargo crate | `cargo:paseo-review@1.2.0` (exact semver), or `cargo:paseo-review` (latest stable non-yanked) |
| Release archive | `release:https://releases.example/review-1.2.0.zip` plus `--version 1.2.0 --sha256 <digest> --format zip` |
| Registry | `owner/slug` or `registry.example/owner/slug` |

Names/URLs in this table illustrate syntax, not existing published plugins. Bare `owner/slug` is a registry identity, not a local path or GitHub shorthand. Prefix relative directories with `./` or `../`. With `--host`, use an absolute path on that daemon; local files are not uploaded. `--path` selects a contained source subdirectory; Git also accepts `github:acme/monorepo:plugins/review`. `--ref` chooses a Git branch/tag/commit. Cargo versions use `@exact-semver` on install, not `--version`.

Release URLs require HTTPS without embedded credentials, an explicit version and lowercase 64-character SHA-256. `--format` is `tar.gz` or `zip`; it can be inferred from recognized URL suffixes. Downloads are bounded/checksummed before safe extraction. Source acquisition is not a sandbox: preparation and validation execute trusted code afterward.

Default registry: `https://plugins.paseo.sh`; `PASEO_PLUGIN_REGISTRY` selects another default. Private registry authorization is configured with `pluginRegistries: {"host":{"authorization":"Bearer token"}}`; credentials are not forwarded to artifact hosts or redirects. Existing installs retain their source/registry identity. A registry must serve native Git/Cargo/release records; this checkout does not prove the public registry has migrated or published any native plugin. npm sources and legacy JavaScript records are not executed; see [migration](migration.md).

`--id` overrides installation identity. Duplicate IDs fail. Removing a directory install preserves user-owned sources; removing a managed install removes its managed files, not arbitrary external directories.

## Preparation commands

Manifest `build` is an array of nonempty argv arrays (as in the manifest above). Commands run directly from the staged plugin directory, without an implicit shell. They may fetch dependencies or execute Cargo build scripts with daemon-user privileges. Installation/update prepares and validates a candidate before activation; failure retains the previous managed installation. No commands are inferred from lockfiles. For reproducible distribution commit `Cargo.lock` and use `cargo build --locked --release --target-dir target`; the generated scaffold's initial command intentionally works before a lockfile exists. Rebuild local directory sources before reload.

## CLI reference

```sh
paseo plugin init /absolute/path/to/plugin --sdk-path /absolute/path/to/paseo/packages/plugin
paseo plugin install /absolute/path/to/plugin
paseo plugin ls review
paseo plugin logs review
paseo plugin update review --check
paseo plugin update review --yes
paseo plugin update --all --check
paseo plugin update --all --yes
paseo plugin update review --version 1.2.0
paseo plugin update review --ref v2
paseo plugin reload review
paseo plugin disable review
paseo plugin enable review
paseo plugin remove review
```

Use the actual installed ID in place of `review`. `install` also accepts `add`. Ordinary updates preview and require `--yes`; `--check` never applies. Explicit `--version` selects a Cargo version; `--ref` selects Git. Explicit selections authorize one target, not `--all`. Release updates require `--url <HTTPS URL> --version <version> --sha256 <digest> [--format zip|tar.gz]` and retain the installed URL origin. They have no implicit latest-release discovery.

Direct Git updates follow default HEAD, Cargo updates choose stable non-yanked releases, and registry updates follow reviewed pins. Directory installs are skipped. Proposals bind immutable artifact identities/checksums; changed proposals fail rather than silently substituting content. Bulk results are independent. `ls` reads installed metadata without querying the remote source.

## Debug backend output

`paseo plugin logs <id>` returns a recent snapshot, not a live follower. Use stderr or `Context::log`; arbitrary stdout corrupts IPC. Never log secrets. Missing binary, wrong architecture, absent execute permission, incompatible requirements, invalid metadata or runtime failure are real load errors: fix the cause, rebuild when appropriate and reload. A legacy runtime needs a native port, not an interpreter setting. `running` is not proof of provider correctness or UI compatibility.
