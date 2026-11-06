---
title: Getting started
description: Build the native Paseo client and Rust daemon from this checkout.
nav: Getting started
order: 1
category: Getting started
---

# Getting started

Paseo Sessions is a native Rust client on **Linux, macOS, Windows, Android and iOS**, with a **Rust daemon and CLI**. Desktop/server hosts run agents; Android and iOS connect remotely. The client includes Markdown, source editing, summary-first diffs, remote folder browsing and read-only script/setup logs, organized as host → directory → session.

Use [this repository's releases](https://github.com/emotionbug/paseo/releases) for native build artifacts; upstream downloads and store listings are separate products. Source builds and redistribution requirements are documented in the repository's `docs/release.md`.

## Prerequisites

Install and authenticate at least one supported provider CLI on the daemon machine. Paseo manages these tools; it does not supply your provider credentials. Git and, for relevant forge operations, an authenticated forge CLI are also needed.

Source builds require Rust/Cargo and the target platform's native toolchain. Desktop builds require the matching OS; iOS requires macOS/Xcode; Android requires a JDK, the Android SDK and NDK. Paseo's build tools, SDKs and native plugin runtime do not require Node or TypeScript. External agent CLIs retain their own prerequisites.

## Desktop app

From the repository root:

```sh
cargo fetch --locked
cargo xtask dev app
```

This builds/runs the native desktop client with an isolated checkout development home, `.dev/paseo-home`, on port `6768`. It does not replace an installed app or its daemon. For an optimized native build use `cargo xtask build-gui linux`, `macos`, or `windows` on the corresponding host. Packaging outputs go under `artifacts/native/`; a local build is not a published signed installer.

## Server / CLI

Build the native runtime and keep its generated assets together:

```sh
cargo fetch --locked
cargo xtask build-daemon
./target/debug/paseo onboard
./target/debug/paseo daemon start
./target/debug/paseo daemon status
```

Use `paseo.exe` on Windows and the corresponding directory when `CARGO_TARGET_DIR` is set. `cargo xtask build-daemon --release` builds an optimized local runtime under the release target directory; optimization does not confer publication provenance.

The normal daemon endpoint is `ws://127.0.0.1:6767/ws`. Configuration and durable state remain under `PASEO_HOME` (default `~/.paseo`), or the explicit CLI `--home`. `paseo onboard` performs setup; bare `paseo` opens CLI chat, not daemon installation. For an isolated development daemon use `cargo xtask dev daemon` instead.

When using a native daemon archive, extract the **whole** archive, keep libraries, skills, notices and runtime metadata alongside the executable, and invoke that executable. An old npm CLI package does not supply this Rust runtime.

## Mobile clients

```sh
cargo xtask build-gui android --arch arm64
cargo xtask build-gui ios
```

Android uses package `sh.paseo.sessions`. Android/iOS outputs are under `artifacts/native/`; without signing/provisioning they are explicitly debug-signed or unsigned, not store submissions. Add the remote daemon's reachable address and credentials in the client. A phone's loopback address is not the daemon machine.

## Docker

Build the native daemon-only image from this checkout:

```sh
docker build -f docker/base/Dockerfile -t paseo:local .
docker run -d --name paseo-native \
  -p 127.0.0.1:6767:6767 \
  -e PASEO_PASSWORD=change-me \
  -v "$PWD/paseo-home:/home/paseo" \
  -v "$PWD/workspace:/workspace" \
  paseo:local
```

Choose a real password. Connect a native client or CLI; there is **no bundled web UI** to open at the HTTP root. Agent CLIs must be supplied separately. Existing registry images are not evidence of a published native migration.

## Runtime scope

There is no product web client, Electron/Chromium, embedded browser/automation, interactive terminal UI or executable React UI-plugin host. Workspace/service scripts still run in daemon-owned PTYs and provide read-only logs. Native backend plugins and the external Rust SDK remain supported.

Recorded Linux execution evidence and unverified platform boundaries are documented in the repository's `docs/testing.md`. Cross-platform CI entries are not successful device tests. Native packages exclude speech recognition, voice conversations and TTS; generic effect-sound playback remains.

See [Connectivity](connectivity.md), [CLI reference](cli.md), [Rust SDK](sdk/index.md), and the checkout's [README](../README.md) / [development guide](../docs/development.md) for connection and development details.
