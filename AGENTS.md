# AGENTS.md

Paseo is an environment for running, monitoring and controlling your local AI coding agents from anywhere. A Rust daemon runs on your desktop or server next to your code; native Rust clients on Linux, macOS, Windows, Android and iOS connect to it directly or through the end-to-end encrypted relay. Your code stays on your machine.

**Supported agents:** Claude Code, Codex, GitHub Copilot, OpenCode, Pi, Antigravity, and Muse Code.

## Repository map

Rust owns the product: the daemon, the native client (Makepad widgets), SDKs, relay, native plugin runtime, and build/release tooling. The unused website is retired.

- `packages/daemon` — Rust daemon and `paseo` CLI: agent lifecycle, WebSocket API, MCP server, providers, desktop host IPC
- `packages/gui` — Native client (`paseo-gui`, executable `paseo-sessions`) for Linux, macOS, Windows, Android and iOS: Makepad shell and feature widgets
- `packages/gui-core` — Client runtime (`paseo-gui-core`): host connections, directory synchronization, persisted client state
- `packages/gui-platform` — OS integration (`paseo-gui-platform`): desktop host, updater, tray, notifications, pickers, mobile bridges and their Android/iOS sources
- `packaging/` — Installers, bundle templates, icons, Android Gradle project, vendored Makepad Android Java, notice supplements and font license records
- `packages/protocol` — Shared Rust wire types, validation and relay cryptography (`paseo-protocol`)
- `packages/client` — Rust client SDK (`paseo-client`)
- `packages/plugin` — Native backend plugin SDK (`paseo-plugin`)
- `packages/relay` — E2E encrypted relay for remote access (`paseo-relay`)
- `packages/highlight` — Rust syntax highlighting library (`paseo-highlight`)
- `xtask` — Cargo development, build, packaging and release commands
- `plugins/` — First-party backend extensions (usage sources, providers); `plugin-examples/` — SDK examples

The native client is a port of the original React Native/Expo and Electron app at commit `c3bac24ccc2df8dcc67d807c805db0efa0817dff`. Behavior, copy and geometry follow that app; doc comments name the original file a widget ports. Read it with `git show c3bac24:packages/app/src/<path>`.

## Docs

`docs/` is the source of truth for system-level and process-level knowledge. **"The docs", "check the docs", or "check the X docs" always mean this directory — not the web.** Look here before fetching anything online; the docs capture gotchas and conventions you cannot derive from the code or external sources.

At the start of non-trivial work, list `docs/` and skim anything relevant to the task.

| Doc                                                                  | What's in it                                                                                                      |
| -------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| [docs/product.md](docs/product.md)                                   | What Paseo is, who it's for, where it's going                                                                     |
| [docs/architecture.md](docs/architecture.md)                         | Rust daemon and native client design, package layering, WebSocket protocol, data flow                             |
| [docs/agent-lifecycle.md](docs/agent-lifecycle.md)                   | Agent states, parent/child relationships, archive semantics, tabs vs archive, subagents track                     |
| [docs/timeline-sync.md](docs/timeline-sync.md)                       | Live stream vs authoritative history, gap recovery, resume, projected pages vs live presentation                  |
| [docs/data-model.md](docs/data-model.md)                             | File-based JSON persistence: agents, config, schedules, project/workspace registries, native client stores        |
| [docs/glossary.md](docs/glossary.md)                                 | Authoritative terminology — UI label wins, no synonyms                                                            |
| [docs/coding-standards.md](docs/coding-standards.md)                 | Commit to a shape, types, errors, density, module structure and native runtime rules                              |
| [docs/design.md](docs/design.md)                                     | Makepad design — theme ownership, reuse, hierarchy, density, states and native evidence                            |
| [docs/forms.md](docs/forms.md)                                       | Rust feature forms — shared controls, owner-scoped state, catalog loading and pending mutations                    |
| [docs/hover.md](docs/hover.md)                                       | Native hit regions — current-frame geometry, nested actions and overlay dismissal                                 |
| [docs/unistyles.md](docs/unistyles.md)                               | Native styling — Rust palette, theme epoch and Makepad widget ownership; filename retained for links              |
| [docs/floating-panels.md](docs/floating-panels.md)                   | Shell-drawn overlays — window/owner targeting, anchors, modal input and feature sheets                            |
| [docs/menus.md](docs/menus.md)                                       | Shared overlay menus/selects — trigger identity, answer ownership and compact presentation                         |
| [docs/expo-router.md](docs/expo-router.md)                           | Native navigation — per-window layout, pane-scoped targets, restore and platform routes; filename retained         |
| [docs/mobile-panels.md](docs/mobile-panels.md)                       | Compact panels — sidebar → session → file view flow and back behavior                                             |
| [docs/explorer-sidebar.md](docs/explorer-sidebar.md)                 | Explorer and supporting panes — file browsing, source/Markdown views and Changes next to sessions                 |
| [docs/notifications.md](docs/notifications.md)                       | Notification identity and delivery, mobile and desktop integration                                                |
| [docs/i18n.md](docs/i18n.md)                                         | English/Korean catalogs, OS locale resolution, interpolation and count fallback                                    |
| [docs/file-icons.md](docs/file-icons.md)                             | File and project icons — Rust presentation, shared decoding and daemon-side project icon discovery                |
| [docs/markdown-renderer.md](docs/markdown-renderer.md)               | Rust Markdown block/render ownership, file identity and virtualized chat presentation                              |
| [docs/native-text-selection.md](docs/native-text-selection.md)       | Source-indexed selection, current-frame reflow and copy/scroll boundaries                                          |
| [docs/providers.md](docs/providers.md)                               | Adding a native agent provider end-to-end                                                                         |
| [docs/forge-providers.md](docs/forge-providers.md)                   | Adding a git forge: daemon boundary, shared wire facts and Rust client presentation                               |
| [docs/custom-providers.md](docs/custom-providers.md)                 | Custom provider config: Z.AI, Alibaba/Qwen, ACP agents, profiles, custom binaries                                 |
| [docs/plugins.md](docs/plugins.md)                                   | Backend extensions — Rust built-ins, native plugin executables, manifests and RPCs                               |
| [docs/hub.md](docs/hub.md)                                           | Paseo Hub — opt-in connection, authority, session grants, revocation                                              |
| [docs/service-proxy.md](docs/service-proxy.md)                       | Service proxy: exposing workspace scripts at public URLs, DNS setup, reverse proxy config                         |
| [docs/relay-endpoint-security.md](docs/relay-endpoint-security.md)   | Relay E2EE modes: Noise NK mapping, record framing, negotiation, the `requireProtectedSecurity` flag, close codes |
| [docs/development.md](docs/development.md)                           | Toolchains, dev commands, isolated state and endpoints, native builds, source ownership                           |
| [docs/rpc-namespacing.md](docs/rpc-namespacing.md)                   | WebSocket RPC naming convention — dotted namespaces and `.request`/`.response` pairs                              |
| [docs/protocol-compatibility.md](docs/protocol-compatibility.md)     | Why app/daemon versions drift, protocol vs feature contract, capability gating, COMPAT tagging                    |
| [docs/protocol-validation.md](docs/protocol-validation.md)           | Rust wire validation, forward-compatible decoding and native validation boundaries                              |
| [docs/connected-hosts.md](docs/connected-hosts.md)                   | Daemon-owned host registrations, routed SDK scopes, identity and authority boundaries                             |
| [docs/permissions.md](docs/permissions.md)                           | Semantic daemon permissions, principals, credentials, pairing invitations, and Hub authority                      |
| [docs/terminal-activity.md](docs/terminal-activity.md)               | Workspace setup and service scripts as daemon-owned PTYs with read-only client logs                               |
| [docs/terminal-performance.md](docs/terminal-performance.md)         | Read-only execution logs — daemon PTYs, ANSI replay, observation slots and visible-row rendering                    |
| [docs/agent-stream-performance.md](docs/agent-stream-performance.md) | Assistant text pipeline from the daemon through the Rust core to the virtualized Makepad timeline                 |
| [docs/file-observation.md](docs/file-observation.md)                 | Daemon file and checkout observation, Linux constraints, teardown invariants                                      |
| [docs/testing.md](docs/testing.md)                                   | Rust, GUI and SDK checks, required native smoke boundaries, Linux desktop regression                              |
| [docs/qa.md](docs/qa.md)                                             | QA evidence bar for pull requests — platform matrix, version drift, performance, UI proof                         |
| [docs/mobile-testing.md](docs/mobile-testing.md)                     | Android/iOS client testing against a separate Rust daemon                                                         |
| [docs/ad-hoc-daemon-testing.md](docs/ad-hoc-daemon-testing.md)       | Running the real Rust daemon against a disposable home and endpoint                                               |
| [docs/browser-capture-harness.md](docs/browser-capture-harness.md)   | Native capture boundary — capturing the real staged Rust client; filename retained for links                     |
| [docs/android.md](docs/android.md)                                   | The Android client: build and launch, signing, versions, F-Droid source builds and store metadata                 |
| [docs/docker.md](docs/docker.md)                                     | Running the native daemon in Docker, volumes, agent images, security                                              |
| [docs/release.md](docs/release.md)                                   | Release playbook, distribution requirements, draft releases, completion checklist                                 |
| [SECURITY.md](SECURITY.md)                                           | Relay threat model, E2E encryption, DNS rebinding, agent auth                                                     |
| [public-docs/hub/security.md](public-docs/hub/security.md)           | Public Hub guide — trust boundaries, untrusted triggers, provider controls, and output authority                  |

### Writing docs

- **Integrate, don't append.** Find the doc that owns the subject and rewrite the part that is now wrong. The standard failure is finishing a task and adding a paragraph to the bottom of the closest-looking doc; ten tasks later the doc is a pile of paragraphs in discovery order. `docs/custom-providers.md` is what that looks like.
- **Don't document logic.** Prose that restates code drifts from the code and loses. Write down what the code can't tell you: why something is shaped the way it is, the gotcha that cost an afternoon, conventions nothing enforces, constraints that span packages or versions. If a reader could get it in two minutes by opening the file, cut it.
- **One fact, one doc.** Every other mention is a link. If you are about to write the same paragraph in two docs, one of them is a link.
- **Respect the layers.** `CONTRIBUTING.md` and this file name things and link out. Activity docs like `docs/qa.md` and `docs/testing.md` set the bar for a kind of work. Subject docs like `docs/timeline-sync.md` own one thing completely. A layer never re-explains the one below it.
- **One subject per doc.** If the subject doesn't fit in a sentence, split the doc. A section per provider, vendor, or platform is a table plus one worked example.
- **Delete.** Obsolete sections go. Prefer a `packages/daemon/src/thing.rs:120` reference over a pasted block.
- **New doc?** Add a row to the table above and link it from the docs that should send readers there.
- Code-level facts belong in comments next to the code, not here.

### Doc voice

Plain and short. Second person. State the rule, then the reason when the reason isn't obvious. Match the doc you're editing.

Do not:

- Write a sentence to land a point. "It's not X, it's Y", "That's not a Z, that's a W", and every other setup-and-punchline shape.
- Add a clause that only asserts importance: "and that matters", "which is what keeps it working", "this is critical".
- Use "honest", "robust", "seamless", "powerful", "simply", "just", "delightful".
- Restate something you already said, in different words, for emphasis.
- Hedge with "generally", "typically", or "you may want to" when the answer is "do this".
- Clear your throat: "It's worth noting that", "In order to", "This section covers".

## Quick start

```bash
cargo xtask dev daemon                 # Start the isolated Rust dev daemon
cargo xtask dev app                    # Build and run the desktop Rust client
cargo xtask dev app android --device <id>
cargo xtask dev cli ls -a -g            # List agents in the dev home
cargo xtask dev cli daemon status
cargo check --locked --workspace --all-targets
cargo clippy --locked -p paseo-gui-core -p paseo-gui-platform -p paseo-gui --all-targets
```

Repo dev commands use checkout-local state. `PASEO_HOME` resolves to `.dev/paseo-home`, the dev daemon listens on `127.0.0.1:6768`, and `cargo xtask dev cli ...` targets that same dev home. The installed daemon and app keep using `~/.paseo` on port `6767`.

When Mo requests an APK, use `cargo xtask build-gui android --arch arm64`: it writes the APK to `artifacts/native/` (debug-signed without a release key) without starting a device. Omit `--arch arm64` for a universal APK.

See [docs/development.md](docs/development.md) for toolchains, isolated state, native builds and source ownership.

## Release branches

When the user says "this goes to next", create or
retarget the PR to `next` and preserve that destination through delivery. Follow
[release branch discipline](docs/release.md#release-branch-discipline) for creating
and updating `next`, integrating it after a release, and releasing a hotfix from a tag.

## Critical rules

- **NEVER restart the main Paseo daemon on port 6767 without permission** — it manages all running agents. If you're an agent, restarting it kills your own process.
- **NEVER assume a timeout means the service needs restarting** — timeouts can be transient.
- **NEVER add auth checks to tests** — agent providers handle their own auth.
- **Before changing navigation, startup routing, remembered workspace restore, or active workspace selection, read [docs/expo-router.md](docs/expo-router.md).**
- **NEVER run the full test suites locally.** The Rust workspace suite is heavy and will freeze the machine, especially if multiple agents run it in parallel. Rules:
  - Run only the tests for what you changed: `cargo test --locked -p <crate> --lib <test-name-filter>`.
  - Never run a whole suite unless explicitly asked, and never run two heavy suites at once.
  - Keep long check output in a captured log rather than flooding the conversation.
  - Never re-run a test suite that another agent already ran and reported green — trust the result.
  - For full suite verification, push to CI and check GitHub Actions instead.
- Add behavioral tests to existing Rust suites and reuse their CI jobs instead of creating feature-specific ones.
- **Check and lint the changed surface.** `cargo check --locked -p <crate> --all-targets` and scoped Clippy.
- Match surrounding Rust formatting; do not mass-reformat unrelated legacy files. No npm formatter or TypeScript build step is required.
- **The protocol stays backward-compatible. Features don't have to.** Read [docs/protocol-compatibility.md](docs/protocol-compatibility.md) before touching the wire protocol in `packages/protocol` or the daemon's message handling. The short version:
  - **Protocol contract (always):** an old client parses messages from a new daemon, and a new daemon parses messages from an old client. New fields are optional; never narrow, never remove, never require. Validation must not silently transform messages or substitute defaults for malformed input.
  - **Feature contract (per-feature):** gate the capability once on `server_info.features.*`, then run the feature or tell the user to update the host. No fallback paths, no defensive branches.
  - **Every shim is tagged.** `// COMPAT(name): added in vX, remove after <date>` at the site that has to be deleted. `rg "COMPAT\("` is the cleanup backlog; untagged back-compat is permanent by accident.
  - **New RPCs use dotted namespaces with direction suffixes.** Follow [docs/rpc-namespacing.md](docs/rpc-namespacing.md): `domain.provider.operation.request` pairs with `domain.provider.operation.response`. Existing flat RPC names will migrate over time; don't add new ones.

## Platforms

The native client targets Linux, macOS and Windows desktops and Android and iOS. Desktop can manage a local daemon through the desktop host; Android and iOS are remote clients and never run a daemon. Code is cross-platform by default. Gate only when you must.

| I need to...                                     | Use                                                                                                        |
| ------------------------------------------------ | ---------------------------------------------------------------------------------------------------------- |
| Switch layout between phone and tablet/desktop   | `common::is_compact_layout(cx)` in `packages/gui/src/common` (width below 600)                             |
| Branch on the operating system                   | `cfg!(target_os = "…")` / `#[cfg(target_os = "…")]`                                                        |
| Use a desktop or mobile platform service         | `paseo_gui_platform::PlatformHandle` (`packages/gui-platform`); see its `desktop` and mobile modules       |
| Show something on hover, always visible on touch | Hover on desktop targets; keep the control visible on Android and iOS (see [docs/hover.md](docs/hover.md)) |

- **Keep the original's platform branches.** The original app's native (iOS/Android) behavior maps to Android and iOS; its web and Electron behavior maps to the desktop targets. A branch that differs from the original is a bug, not a simplification.
- **Don't use the platform as a proxy for layout.** A narrow desktop window is compact; use `is_compact_layout`, not `target_os`.
- **The native product excludes** the interactive terminal UI (daemon PTYs and read-only script/setup logs stay), the web client, embedded browser and Electron, executable React UI plugins, speech recognition, voice conversation and TTS. Generic effect-sound playback stays. Don't reintroduce the excluded features.

## Debugging

Find the complete daemon logs and traces in `$PASEO_HOME/daemon.log`.
