---
title: Plugin quickstart
description: Build and manage a trusted native backend extension with Rust.
nav: Quickstart
order: 44
category: Plugins
---

# Plugin quickstart

A Paseo plugin is a **standalone native executable** on the daemon host. The Rust `paseo-plugin` SDK supplies bounded newline-JSON IPC, RPC handlers, lifecycle hooks, settings, provider connections, usage sources and the daemon-client bridge. It is not a Rust dynamic library and does not run inside the client.

This guide describes the **0.13.1 source checkout, not a published release**. There is no JavaScript worker, embedded compiler or Node fallback. Existing JavaScript plugins need an [explicit port](migration.md). The native client does not execute plugin screens, React components, panels, themes or timeline renderers; mobile connects remotely and does not host plugins.

## Prepare the daemon host

Use Rust 1.93 or newer (edition 2024) to build source plugins. Git sources also require Git. Prebuilt native plugins need the correct operating system, architecture and runtime libraries, not a Rust compiler merely to execute them. Build the checkout daemon with `cargo xtask build-daemon`; use an isolated development daemon, never an existing production installation for experiments.

Plugins and their preparation commands are **trusted, unsandboxed code**, with the daemon user's file, process, network and credential access. Validation itself runs the executable. Review the source, dependencies, artifacts and updates before installing.

## Create a plugin

Use a local SDK checkout until the desired SDK revision is actually available remotely:

```sh
paseo plugin init /absolute/path/to/workspace-plugin --sdk-path /absolute/path/to/paseo/packages/plugin
cd /absolute/path/to/workspace-plugin
cargo build --release --target-dir target
./target/release/paseo-plugin-server --validate . 0.13.1
```

On Windows the scaffold selects `target/release/paseo-plugin-server.exe`; invoke `.\target\release\paseo-plugin-server.exe`. When moving a project between operating systems, update `server.executable` accordingly.

The scaffold creates `Cargo.toml`, `src/main.rs`, `paseo-plugin.json`, `README.md`, `OVERVIEW.md` and `.gitignore`. It registers the `greeting.create` RPC: input `{"name":"World"}` returns `{"message":"Hello, World!"}`. It does not download dependencies or compile at creation time. Without `--sdk-path`, its SDK dependency points to the Paseo Git repository; pin a revision containing the required crate before distribution, rather than assuming crates.io publication.

The manifest uses `runtime: "native"`, a contained `server.executable`, and `server.args: []`. The scaffold declares a Cargo preparation command. See the [reference](reference.md) for the complete contract.

## Install and try it

On a selected **test daemon**:

```sh
paseo plugin install /absolute/path/to/workspace-plugin
paseo plugin ls workspace-plugin
paseo plugin logs workspace-plugin
```

The daemon-wide `pluginsEnabled` setting defaults to `true` when omitted; set it to `false` to disable all external plugins. Each installation's enabled flag must also allow execution. Explicit global and per-installation opt-outs are preserved. After changing the root setting in `config.json`, use `paseo reload --json`; `paseo plugin enable workspace-plugin` changes only that installation. A `running` status means activation succeeded, not that every RPC/provider path works or that an old plugin UI is supported.

For a remote daemon, put `--host <url>` before the management command and use an absolute plugin path **on that host**. The CLI does not upload your local files. `plugin init` creates files locally.

## Run the repository example

From the repository root on Linux/macOS:

```sh
cargo build -p paseo-workspace-inspector --target-dir plugin-examples/workspace-inspector/target
./plugin-examples/workspace-inspector/target/debug/paseo-workspace-inspector --validate plugin-examples/workspace-inspector 0.13.1
paseo plugin install ./plugin-examples/workspace-inspector
paseo plugin ls workspace-inspector
paseo plugin logs workspace-inspector
```

On Windows, append `.exe` to the example manifest's `server.executable` and invoke `.\plugin-examples\workspace-inspector\target\debug\paseo-workspace-inspector.exe` with the same validation arguments.

The [example implementation](../../plugin-examples/workspace-inspector/src/lib.rs) is a real local-file inspector, not an AI service:

- `file.inspect` accepts `{"cwd":"/absolute/workspace","path":"README.md"}` and returns bounded UTF-8 file statistics and a preview. Direct RPC reads are explicitly requested by the caller and do not create a provider permission prompt.
- `lifecycle.counts` reports the current process's observed `agent.created` events.
- `inspection` settings persist `maxBytes` (default 65536, range 1–1048576). The session-open hook adds `PASEO_INSPECT_MAX_BYTES` to launch overrides.
- Select provider `workspace-inspector` and model `local-file`, then send a relative file path or its `inspect` command. Approve or deny the read request. The example rejects traversal, symlinks, oversized and non-UTF-8 files; it is not a sandbox against malicious concurrent filesystem changes.
- Persistence, steering, configuration changes, images and output schemas are not advertised by this example.

## Edit and reload

Rebuild into the path named by the manifest before reloading:

```sh
cargo build --release --target-dir target
paseo plugin reload workspace-plugin
paseo plugin ls workspace-plugin
paseo plugin logs workspace-plugin
```

These commands are for the scaffold directory; use the example's debug build command for `workspace-inspector`. Reload starts a new executable; it does not compile Rust on demand. Register cleanup for owned tasks, processes, subscriptions, files and sockets, and honor cancellation.

## Install and update shared plugins

Directory, Git, Cargo crate, checksum-pinned HTTPS release archive and compatible registry sources are supported. The source and update flags are documented in the [reference](reference.md#plugin-sources); distribution and trust requirements are in [publishing](publishing.md).

```sh
paseo plugin update workspace-plugin --check
paseo plugin update workspace-plugin --yes
```

Directory sources are edited and reloaded, not remotely updated. Ordinary managed updates require `--yes`; `--check` only previews. No installation, publication or deployment is performed by reading these instructions.
