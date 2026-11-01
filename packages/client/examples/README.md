# Paseo Rust SDK examples

[`inspect.rs`](inspect.rs) is a runnable, read-only native example. It connects to an existing daemon, prints server metadata, measures ping latency, and walks agent cursor pages without creating agents or launching a daemon.

```sh
cargo run -p paseo-client --example inspect -- ws://127.0.0.1:6767/ws
```

Run from the repository root. Omit the endpoint argument to use `PASEO_HOST` or local discovery under `PASEO_HOME`; do not set both environment variables ambiguously. `PASEO_PASSWORD` or the matching local credential supplies daemon authentication. Output contains daemon/agent metadata: treat it as private. This command is suggested usage, not a claim that it has been run.

Further examples are embedded in the repository SDK guides, with checkout path dependencies and a Tokio entry point in the quickstart. They use `paseo_client::{DaemonClient, ConnectOptions, DaemonTarget, RequestOptions}` and protocol-shaped `serde_json::Value` arguments. No registry publication is assumed.

- [Quickstart](../../../public-docs/sdk/quickstart.md): connect, create an agent, wait for the turn, close on success/error.
- [Agents](../../../public-docs/sdk/agents.md): follow-up turns, labels, parentage, structured output and archival.
- [Workspaces](../../../public-docs/sdk/workspaces.md): fresh workspaces/worktrees, placement, terminals and cursor pagination.
- [Providers](../../../public-docs/sdk/providers.md): directory-sensitive discovery, models, modes, features and compact snapshots.
- [Events](../../../public-docs/sdk/events.md): owned observations, timeline history, permission responses and reconnect gaps.
- [Recipes](../../../public-docs/sdk/recipes.md): issue integration, parallel review, resident roles and cleanup after partial failure.
- [Provider options](../../../public-docs/sdk/provider-options.md): daemon defaults and per-agent overrides.
- [Reference](../../../public-docs/sdk/reference.md): remote/relay/SSH authentication, errors, binary transport and highlighting.

Each guide has a linked Chinese translation. Async function snippets are intended to be called inside the quickstart's connected-client lifetime, not run as standalone scripts. Paths are on the daemon host, and model IDs come from discovery. Examples that create agents or execute terminal commands perform real work and may incur provider usage; use an isolated development daemon and archive only resources you own.

Provider profiles, environment variables, custom binaries and extra models remain daemon-configuration operations exposed by `get_daemon_config` and `patch_daemon_config`; there is no separate provider-settings facade. Preserve credentials when inspecting/updating configuration and do not print secret values.
