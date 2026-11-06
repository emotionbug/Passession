# Git forge integrations

Forge operations are daemon behavior. `packages/daemon/src/git/forge.rs` and `git/forge/` own service behavior; Rust presentation belongs to `packages/gui/src/features/git` and forge attachments in `features/composer`. `packages/protocol` owns the shared wire metadata consumed by the daemon and Rust clients.

Do not add a TypeScript server adapter or React client registry in a removed application tree. Keep forge-specific logic behind the current native boundary rather than branching by vendor throughout workspace orchestration.

## Wire contract

Shared messages carry forge-neutral facts and open service-specific metadata. `forgeSpecific.forge` identifies the **facts family**, not necessarily the visible brand: Gitea, Forgejo and Codeberg may share Gitea-shaped facts while retaining distinct top-level brand IDs.

Unknown or schema-mismatched service-specific facts should render neutrally without rejecting an otherwise valid message. Check results preserve normalized status plus optional traits and raw native detail. Do not put one service's vocabulary into every consumer's normalized status enum.

Preserve existing compatibility fields for supported peers and keep their serialization policy at one boundary. Replacing the client renderer does not authorize changing persisted IDs, URLs or request names. The protocol schemas and external SDK must stay aligned when adding a capability.

## Native implementation

The adapter owns authentication, command/API invocation, pagination, facts validation, error mapping and mutation semantics. A cached status is not authority for a destructive mutation: confirm the current remote identity/state at the owning operation. Keep host/cwd and repository identity explicit; an equal path on another host is unrelated.

Client presentation owns labels, links, status and action availability. Open external forge links outside the app; there is no embedded browser or browser automation fallback. Use the forge's own PR/MR terminology in localized copy.

## Verification

Exercise real read and mutation boundaries with authorized test repositories/credentials. Retain errors and service-native evidence; a fixture validates decoding, not authentication or remote effects. Use isolated daemon state and bounded queries so a large repository does not cause one process/request per visible row.

Use scoped Rust checks from [testing](testing.md). `cargo xtask build-daemon` stages native runtime assets; `cargo check --locked -p paseo-client` checks the Rust SDK. Exercise affected Git/forge calls against an isolated daemon rather than treating compilation as interoperability evidence.
