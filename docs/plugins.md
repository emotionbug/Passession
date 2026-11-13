# Backend extensions

Paseo's daemon, built-in providers and usage integrations are Rust. Third-party backend plugins are **standalone native executables** using bounded JSONL IPC, not Rust dynamic libraries, JavaScript workers or interpreted shims. The Rust client does not execute plugin UI. This describes the source contract, not publication or deployment evidence. Kiro coding-agent provider enablement is separate from the optional `omp-kiro` account-usage source.

## Runtime and source ownership

- Management, persistence, hooks, RPC and provider bridge: [daemon plugins](../packages/daemon/src/plugins.rs) and [worker supervisor](../packages/daemon/src/plugins/worker.rs).
- Manifest/path/native executable validation: [manifest.rs](../packages/daemon/src/plugins/manifest.rs).
- Public author SDK: [`paseo-plugin`](../packages/plugin/src/lib.rs), with provider traits, settings, usage types/helpers and daemon-client bridge.
- External Rust client: [packages/client](../packages/client/Cargo.toml); shared protocol: [packages/protocol](../packages/protocol/Cargo.toml).
- Runnable native example: [workspace-inspector](../plugin-examples/workspace-inspector/src/lib.rs), crate `paseo-workspace-inspector`.

Source builds use Rust 1.93+, edition 2024, and `cargo xtask build-daemon`. There is no Node/npm compiler, worker asset or runtime fallback. External agent CLIs remain separate products with their own dependencies.

## Native executable contract

`paseo-plugin.json` requires `runtime: "native"`, `server: {"executable":"relative/native/binary","args":[]}`, an ID and a compatible explicit `requirements.paseo`. The entry must be a regular ELF/Mach-O/PE binary inside the plugin directory after symlink resolution, executable on Unix. Scripts, traversal and interpreter shims are rejected. Match the host OS/architecture and Windows `.exe` suffix.

The host launches the binary with `server.args`; validation appends `--validate <absolute-directory> <daemon-version>`. `Plugin::run` prints one bounded validation object with `valid: true`, manifest `plugin.id`, `methods`, `providers`, `usageSources` and `hooks.events`/`hooks.before`. Normal `initialize` is followed by `ready` carrying the registration fields. Stdout belongs exclusively to bounded JSONL; diagnostics use stderr or `Context::log`.

Validation executes native code and is not a sandbox. Installation, build scripts, dependencies and updates all run with daemon-user filesystem, network, credential and process privileges. Only install trusted code. Mobile clients remotely use the daemon and never run the executable themselves.

## Authoring and management

The CLI scaffold creates Rust source, Cargo/manifest configuration and author docs. For this unpublished SDK, use a local dependency:

```sh
paseo plugin init /absolute/path/to/native-plugin --sdk-path /absolute/path/to/paseo/packages/plugin
```

Without `--sdk-path`, the generated SDK dependency uses the Paseo Git repository; pin an available native-crate revision for distribution. Build the scaffold with `cargo build --release --target-dir target`, then invoke its `target/release/paseo-plugin-server` (`.exe` on Windows) with `--validate . 0.13.1`.

Directory, Git, Cargo crate, checksum-pinned HTTPS release and compatible registry sources remain host-side. See the [source/CLI reference](../public-docs/plugins/reference.md#plugin-sources) for exact selectors, preparation, update proposal approval and artifact identities. Remote paths must exist on the selected daemon host; CLI installation does not upload local files.

The root `pluginsEnabled` setting defaults to `true` when omitted; an explicit `false` still disables all external plugins. Each installation's enabled flag also gates execution, so a disabled installation stays disabled. Directory reload starts a freshly built executable; it does not compile Rust. `plugin logs` returns a recent snapshot, not a live follower. Installation status is not proof of end-to-end behavior.

## Contributions retained

- `Plugin::rpc`: async JSON handlers; authors explicitly validate application input/output.
- `Plugin::on`/`before`: lifecycle notifications and allowed request transforms, preserving cancellation and ordering.
- `Plugin::settings`: host-local versioned documents, migrations, revisions/conflicts and change subscriptions.
- `Plugin::provider`: negotiated connections, separate send acceptance and events, permissions, sessions, timeline and optional persistence/configuration.
- `Plugin::usage_source`: scoped account discovery and usage retrieval, with typed helpers under `paseo_plugin::usage`.
- `Context::daemon.client`: lazy Rust client over IPC for ordinary daemon operations, not a second token/network configuration.
- `Plugin::cleanup`: bounded cleanup of resources the plugin owns. The supervisor tracks and shuts down its process; crashes fail pending operations rather than reporting success.

Details and complete author examples: [reference](../public-docs/plugins/reference.md), [provider guide](../public-docs/plugins/providers.md). Direct adapters implement `Provider`/`Connection`; ACP adapters register `paseo_plugin::acp::AcpProvider` with a command or owned stream and optional `AcpTransformer` extensions. The native helper owns ACP transport/protocol mapping, sessions, permissions and cancellation instead of requiring every plugin to reimplement them. This replaces the old `runAcpProvider` functionality without retaining its JavaScript API or worker. Existing daemon ACP integrations remain native.

## Client compatibility boundary

Protocol-defined configuration controls and standard timeline items are rendered by the Rust settings and timeline features. Executable screens, workspace/file panels, sidebar/header components, custom timeline renderers, composer components, attachment pickers and theme code are not supported plugin surfaces. Historical examples/screenshots and former SDK exports do not imply compatibility. No empty native backend or browser fallback turns a client-only plugin into a useful native extension.

## Explicit JavaScript migration

Legacy manifests/sources fail with an actionable migration error; old installation records/settings are not silently deleted or enabled. Port backend behavior to Rust, preserve IDs/data contracts, and remove old entries/compiler dependencies from the newly distributed plugin. `PASEO_PLUGIN_NODE`/`PASEO_PLUGIN_WORKER` do not restore execution.

There is currently no atomic same-ID managed-source replacement command. Duplicate installs fail, and removal deletes settings: **do not recommend remove/reinstall as data-preserving migration**. The [migration guide](../public-docs/plugins/migration.md#5-change-acquisition-and-build-requirements) documents an explicit directory-runtime migration using the existing ID/configuration, retaining old settings and managed receipts. That does not migrate the managed source record itself, which may still report a legacy-source error.

## Developing and checking a backend extension

Use an isolated test daemon/home, never a running production installation. From the repository root on Linux/macOS:

```sh
cargo build -p paseo-workspace-inspector --target-dir plugin-examples/workspace-inspector/target
./plugin-examples/workspace-inspector/target/debug/paseo-workspace-inspector --validate plugin-examples/workspace-inspector 0.13.1
paseo plugin install ./plugin-examples/workspace-inspector
paseo plugin ls workspace-inspector
paseo plugin logs workspace-inspector
```

Windows needs the example manifest/executable `.exe` suffix. The example implements `file.inspect`, `lifecycle.counts`, `inspection.maxBytes` settings, an agent-created event, a session-open environment transform and the permission-gated `workspace-inspector`/`local-file` provider. It is not an AI service and does not advertise persistence, steering, configuration, image or output-schema support.

Exercise actual initialization, validation, RPC results/errors, settings migrations/conflicts, cancellation, restart and shutdown. Provider checks need a real turn and permission allow/deny path, not only registration. Usage checks must preserve scope/credential boundaries. No tests or live execution are claimed by this document; see [testing](testing.md) for recorded coverage and [distribution](../public-docs/plugins/publishing.md) for packaging requirements.
