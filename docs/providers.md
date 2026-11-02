# Providers

The product daemon and built-in provider integrations are **Rust**. Provider executables remain external tools installed and authenticated on the daemon machine. Their own runtime requirements do not imply that Paseo bundles Node.

Optional backend provider extensions use native executables and the Rust `paseo-plugin` SDK. See [plugins](plugins.md). Existing JavaScript extensions require porting; there is no external-Node worker fallback.

## Native ownership

`packages/daemon/src/providers` owns the native provider boundary:

- `api.rs`: `AgentProvider`, `AgentSession`, `ProviderSpec`, `LaunchContext`, event sinks and tool execution authority.
- `registry.rs` and `registry/`: provider registration/configuration, catalog snapshots, client ownership, options, discovery and shutdown.
- `options.rs` and the options schema: validation of provider-specific configuration.
- `create_config.rs`: default creation policy, resolved mode and unattended behavior.
- Native adapter modules: provider transport, prompt shaping, streaming, permissions, persistence, import/resume and model/mode support.

Agent orchestration, canonical timeline persistence and client publication belong to `packages/daemon/src/agent` rather than to a UI-specific provider implementation. Usage integrations live under `packages/daemon/src/usage`.

Shared Rust wire metadata lives under `packages/protocol` for the daemon and Rust client SDK. Keep that contract aligned with Dart when adding a provider capability or changing serialization.

## Provider contract

A provider owns its native session identifier, opaque persistence handle, native options and actual capabilities. Preserve existing persisted shapes and IDs when changing the implementation. Client disconnect or closing a chat tab must not close the provider session or archive it.

The session API includes turn start/steering, history, runtime info, modes/model/thinking options, permissions, interruption, commands, rewind and close. An unsupported operation must agree with the advertised capabilities and return an explicit error; successful registration alone does not demonstrate that a provider is usable.

Event sinks are bounded and cancellation-aware. The agent owner persists events before outward publication. Never drop an event because a socket or extension consumer is slow, and never publish durable success before the corresponding mutation is persisted.

Parent history replay announces child agents by descriptor only: an `upsert` with an opaque `source` locator. It never reads a child's separate transcript, thread or file. Read one child's body in `AgentProvider::stream_subagent_history` when the user opens it; that call works for a stored parent without resuming it and must fail, not return an empty replay, when the body is missing or unreadable. Child output the parent's own transcript or native stream already carries keeps flowing with it. See [timeline sync](timeline-sync.md#provider-child-history) for how the daemon stores and pages it.

`LaunchContext` binds tools to the agent's authority and environment. Provider options are not an alternate channel for replacing that authority. Exact MCP preapproval must only be advertised when supported; broad “allow all” behavior is not a substitute for an exact tool policy.

## Catalog and configuration behavior

Keep provider models, modes and configuration scoped to the effective environment and working directory. Native catalogs may be discovered asynchronously; distinguish pending discovery, unsupported discovery, and a real provider error. Configuration changes must not return a snapshot from a different identity/environment.

Discovery a provider repeats while opening sessions or answering draft model/command reads goes through the provider instance's `Memo` (`packages/daemon/src/providers/registry/catalog/memo.rs`), not a process-wide cache. Key it on the launch identity the discovery actually talks to: command and arguments, environment without `PASEO_AGENT_ID`, working directory, and a fingerprint of the config, credential and skill/extension paths the native CLI reads. Return failed or degraded lookups as `Err` or `Loaded::Partial` so the next open retries, and clear the memo when `is_available` receives a forced refresh (`force: true`), because that call runs before any catalog reuse. Replacing a provider's configuration replaces its instance and therefore its memo.

CLI-default profiles are host-owned. Explicit command-line provider/model arguments override the saved default; conflicting provider/model values are rejected before connection. Selecting a different provider must not carry over incompatible saved options.

Provider helpers that outlive one session belong to the daemon's managed-process lifecycle. Record and verify process-start identity before signalling a PID. Graceful shutdown, forced termination, restart, and cancellation must not affect another process that reused the same PID.

## Adding an integration

1. Decide whether the integration is a native built-in or an optional native backend extension.
2. Reuse the native provider interfaces and appropriate transport implementation. Keep protocol-specific details behind the provider boundary rather than branching throughout agent orchestration.
3. Add the provider definition/catalog/options and preserve stable wire IDs. Update Rust protocol definitions and Dart presentation where the capability is client-visible.
4. Preserve import/resume, native session identity, permissions, model/mode discovery, cancellation, errors and persistence behavior. Do not claim unsupported features.
5. Update usage integration only if the provider supplies real attributable data; missing data must not become a fabricated zero balance.
6. Add behavior checks for the actual boundary and run them in an authorized verification pass.

Provider UI belongs to native Flutter features under `packages/flutter_app/lib`, not a new TSX icon/registry in a retired client tree.

## Verification

Use an isolated native daemon and real provider credentials for provider end-to-end checks. Exercise a turn, permission decision, interruption, reconnect/history, import/resume and shutdown where supported. Report unavailable credentials/services explicitly. A mock provider can verify controlled states, but it cannot establish real provider interoperability.

The historical Linux scenarios exercised Rust adapters and daemon against controlled Claude/ACP/MCP provider-protocol boundary fixtures, plus the now-retired external-Node extension runtime. They were not paid live Claude/ACP LLM executions and do not verify the new native plugin runtime, live account authentication, every provider mode or other operating systems. See [testing](testing.md) for recorded evidence and its authority boundaries.

The separate [custom-provider configuration reference](custom-providers.md) is not changed by this migration documentation task; source implementation ownership for built-ins is the native path above.
