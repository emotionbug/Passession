---
title: Distribute a plugin
description: Package native plugins as Git sources, Cargo crates or checksum-pinned releases.
nav: Publishing
order: 45
category: Plugins
---

# Publish a plugin

Start with a working [native plugin](index.md). This guide describes the **0.13.1 checkout's distribution contract**, not an announcement that Paseo, its SDK crates, a registry listing or your plugin has been published. There is no npm acquisition or JavaScript runtime fallback. Use native Git sources, Cargo crates or HTTPS release archives; registry records may select those sources.

Only backend behavior is extensible: RPC, hooks, settings data, providers and usage. Flutter does not execute plugin screens, panels, themes or custom React timeline renderers. Document real behavior, supported operating systems/architectures, external agent dependencies and limitations.

## Your listing page

Keep `OVERVIEW.md` beside `paseo-plugin.json` for a concise description, and retain build/install details in `README.md`. The scaffold creates both. Include them in the distributed source/archive, along with the manifest, executable or source/build inputs, license notices and referenced assets.

Explain what the plugin does, its accounts/configuration, data access, network destinations and required tools. Prompts/tool results sent to an upstream provider leave the host even if session history is stored locally. Never imply sandboxing. Explain software downloads and obtain explicit user consent before downloading or installing external tools, including during activation or connection tests.

English and Chinese overviews can be shipped as `OVERVIEW.md` and `OVERVIEW.zh-CN.md`. Manifest listing metadata may include `translations` keyed by language tag with `name`/`description`; daemon execution does not localize or validate registry publication policy for these fields. Keep translations synchronized and check the actual registry's current intake requirements rather than assuming the former website/review bot supports native artifacts.

## Icons and screenshots

Optional `name`/`description`, PNG `icon`, and `media` remain manifest listing fields:

```json
{
  "id": "review-tools",
  "name": "Review tools",
  "description": "Reviews local changes",
  "runtime": "native",
  "requirements": { "paseo": ">=0.13.1" },
  "server": { "executable": "bin/review-tools", "args": [] },
  "icon": "assets/icon.png",
  "media": ["assets/result.png"],
  "translations": {
    "zh-CN": { "name": "审查工具", "description": "审查本地改动" }
  }
}
```

This example is a prebuilt Unix layout; ship the actual binary at that path and choose `.exe` for Windows. Local paths are manifest-relative and contained within the plugin. `media` may also use HTTPS URLs. Provider/usage SVG metadata has its own [reference rules](reference.md#providers), separate from listing PNGs. Use sample data in screenshots and redact private information. Historical client screenshots are not evidence that custom UI works in Flutter.

## Share through GitHub or Git

Commit the manifest, Rust source, `Cargo.toml`, `Cargo.lock`, assets and overview. Source distributions declare explicit preparation:

```json
{"build":[["cargo","build","--locked","--release","--target-dir","target"]]}
```

The executable named by `server.executable` must match the actual binary and output directory. The daemon host needs a compatible Rust toolchain, native build dependencies and Git. No commands are inferred from the lockfile. The scaffold initially omits `--locked` because its first build has not generated a lock; commit the resulting lock and change preparation before distributing reproducible source.

Replace local SDK path dependencies with a Git dependency pinned to an available revision containing the native crate, or a verified published version when one actually exists. Do not substitute a nonexistent crates.io dependency or claim `0.13.1` was published. Source archives cannot depend on files outside the distributed project.

The following selectors illustrate syntax; substitute a real repository and reviewed revision:

```sh
paseo plugin install github:acme/paseo-review --ref v1.2.0
paseo plugin install git:https://git.example/acme/review.git --ref v1.2.0
paseo plugin install github:acme/monorepo --path plugins/review --ref v1.2.0
```

Private Git access uses the daemon host's Git configuration. Review immutable commits rather than trusting a mutable branch name. Direct Git updates follow default HEAD unless an explicit `--ref` selects a target; the initial tag does not freeze future ordinary updates.

## Cargo source packages

The native source selector is `cargo:<crate>[@<exact-semver>]`. With no version, acquisition selects the latest stable non-yanked crates.io version; exact prereleases are allowed when explicitly selected. Acquisition records the exact version, registry checksum and canonical crate download URL. This is **source acquisition**, not `cargo install` of a globally installed executable.

Include `paseo-plugin.json`, all Rust source and assets, and the lockfile needed by `--locked` preparation in the published crate. Check Cargo package inclusion rules: workspace-relative dependencies and omitted assets can make a locally working project unusable after packaging. The host safely extracts, runs declared preparation, then validates the executable inside the staged plugin directory. Use an explicit supported host target; a single Unix manifest does not automatically select a Windows executable.

After actually publishing and inspecting a package through your normal release process, users can install its exact version, for example `paseo plugin install cargo:paseo-review@1.2.0`. That name/version is illustrative, not a claimed available package. Update with `paseo plugin update review --version 1.2.1` for an explicit target, or preview ordinary latest selection with `--check`.

## Prebuilt release archives

Ship one appropriate native binary per host platform/architecture, plus its `paseo-plugin.json` and assets. Prebuilt-only distributions should omit unnecessary source build commands. Preserve Unix executable bits in tar archives. An archive may contain a plugin subdirectory selected with `--path`; it must not escape the extracted root. Acquisition rejects traversal, links/special entries, duplicate paths and oversized contents.

The install contract is:

```text
paseo plugin install release:<HTTPS-archive-URL> --version <semver> --sha256 <64-character-lowercase-digest> --format <tar.gz|zip> [--path <plugin-subdirectory>]
```

Compute the SHA-256 over the **archive bytes**, not the executable alone. Supply the digest from a trusted release channel. HTTPS without URL credentials is required; `--format` may be omitted only when a recognized URL suffix identifies the format. The host downloads with bounds, verifies the digest, extracts and validates before activation. These controls prevent accidental substitution/path escapes; they do not make the executable trustworthy or sandbox its build scripts.

Release sources do not discover a latest version. Preview an explicit next archive using:

```text
paseo plugin update <installed-id> --url <HTTPS-archive-URL> --version <semver> --sha256 <digest> --format <tar.gz|zip> --check
```

Omit `--check` only after reviewing the exact target; explicit selection authorizes that target. The URL must retain the installed origin. Registry-managed releases instead follow the registry's reviewed artifact target. Ordinary updates require `--yes`; exact artifact identity is bound into the proposal, and changed proposals fail rather than installing different content.

## Plugin registry

Registry IDs remain `owner/slug` (or a registry-host prefix). A compatible registry document must provide native source identity, not an old npm pin:

- Git: the existing reviewed commit and contained plugin path contract.
- Cargo: `artifact` with `kind: "cargo"`, `crate`, exact `version`, `checksum`, canonical `resolved` URL, and optional `pluginPath`.
- Release: `artifact` with `kind: "release"`, HTTPS `url`, `version`, `sha256`, `format`, and optional `pluginPath`.

Keep the document's `id` consistent with the requested registry ID. Installation records preserve source identity and full artifact receipts; updates are pinned to reviewed immutable targets. See [source management](reference.md#plugin-sources) for registry configuration and credential boundaries.

This repository's native consumer support does not prove that a public registry has accepted the new schema, migrated its listings, or deployed an intake service. Confirm the registry's native support before advertising `paseo plugin install owner/slug`; distribute through direct native sources in the meantime. Do not reuse the former npm publication/submission instructions.

## Validate behavior before distribution

On an isolated test host, build for the claimed target, invoke the executable's `--validate <directory> <daemon-version>`, then install and exercise the actual RPCs, settings migration/conflicts, hooks, usage discovery and provider permission/turn/persistence paths you advertise. Exercise reload/disable/removal while work is active. Keep logs free of credentials. Validation runs trusted executable code and only checks startup/registration, not complete behavior or safety.

Distribute only platform support you have actually exercised, preserve data and installation IDs across compatible updates, and disclose source/build/dependency trust. Publication and deployment require your own deliberate release action; none is asserted here.
