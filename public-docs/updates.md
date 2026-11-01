---
title: Updates
description: Native build identity, update channels, and migration availability.
nav: Updates
order: 5
category: Getting started
---

# Updates

The checkout version is **0.13.1**. Use [this repository's releases](https://github.com/emotionbug/paseo/releases) and their recorded source revision to identify published artifacts. Upstream downloads, legacy npm CLI packages, store listings and container `latest` tags do not establish this fork's release identity.

## Native build identity

The product client is Flutter on Linux, macOS, Windows, Android, and iOS. Desktop/server hosts run the Rust daemon and Rust `paseo` CLI; mobile clients connect remotely and do not run a daemon.

For a local source build, use Rust/Cargo, Flutter, and the target platform's native tools:

```bash
cargo fetch --locked
cargo xtask build-daemon
# Or an optimized LOCAL build, not release provenance:
cargo xtask build-daemon --release
cargo xtask build-flutter linux
```

Choose `linux`, `macos`, or `windows` on the matching desktop host. Flutter outputs are under `artifacts/flutter/`. The daemon is `target/debug/paseo` or `target/release/paseo` (`paseo.exe` on Windows), unless `CARGO_TARGET_DIR` overrides the root. Keep generated adjacent assets with the executable.

## Updating a local installation

Back up your Paseo home before replacing a runtime. The default remains `~/.paseo`, including existing configuration and history. Stop the managed daemon with `paseo daemon stop`, replace the complete runtime with a build from the intended revision, then run `paseo daemon start` and `paseo daemon status`. A worker restart is not a replacement of the supervisor executable.

Bare `paseo` opens CLI chat. Use `paseo onboard` for explicit setup, not as an update command.

## Channels and publication

Stable and beta are release identities, not proof that this migration has shipped. Optimizing a local binary does not enable official updater provenance. Native publication requires validated source identity, channel/repository metadata, complete runtime artifacts, signing where applicable, and an authorized release.

Do not rely on legacy desktop update-screen instructions for a local Flutter build. See the repository [release guide](../docs/release.md) for current update manifests, rollout controls, and packaging requirements.

## Mobile and verification boundaries

`cargo xtask build-flutter android` or `cargo xtask build-flutter android --arch arm64` builds the Android remote client. Without release signing credentials, output is explicitly debug-signed. `cargo xtask build-flutter ios` requires macOS/Xcode and emits an unsigned device app archive unless provisioning/export is supplied. Neither output is automatically store-ready.

See [testing evidence](../docs/testing.md) for recorded execution and platform limitations. Historical compilation or a CI workflow definition does not prove that a new release was installed or exercised on a device.

Preserve generated Cargo and bundled-asset notices when distributing native packages. Notices are not a compliance certification or corresponding-source offer; review applicable dependency obligations. The [release guide](../docs/release.md) owns distribution requirements. Speech runtimes and model downloads are no longer included.
