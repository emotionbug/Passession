# Vendored libghostty-vt-sys

`libghostty-vt-sys/` is the published `libghostty-vt-sys` 0.2.2 crate
(libghostty-rs commit `c9876b2902d5bb3e1f3d61b6395059f1e5e7b721`), patched into
the workspace through `[patch.crates-io]`. `src/` and `LICENSE` are unmodified;
the generated bindings still target Ghostty commit
`a887df42c56f6de86c0fe6da9c4eeca37931e083`, which builds with Zig 0.15.2.

`build.rs` changes:

- `ZIG` selects the Zig executable when it is not on `PATH`.
- Android slices pass `.24` as Zig's Android API level. Zig otherwise defaults
  bionic targets to API 29, above the client's minimum SDK.
- iOS targets build through Ghostty's `-Demit-xcframework` path on a macOS host
  and link the slice's static archive. `x86_64-apple-ios` has no Ghostty slice.
- 32-bit Android fails with an explicit message: the checked-in bindings assert a
  64-bit layout.
- The Ghostty clone enables `core.longpaths` for Windows.
- Cargo debug profiles (`dev`, `dev-fast`) build Ghostty `ReleaseSafe` instead of
  `Debug`. Ghostty's Debug mode runs page-list integrity scans on every mutation,
  which stalled a PTY printing a few thousand lines past a 10-second test timeout;
  `ReleaseSafe` keeps Zig's runtime safety checks. Release profiles stay
  `ReleaseFast`, and `LIBGHOSTTY_VT_SYS_OPTIMIZE` still overrides both.

The binding generator binary and its optional dependencies are removed.

The build clones Ghostty into Cargo's `OUT_DIR` unless `GHOSTTY_SOURCE_DIR`
points at a checkout of the pinned commit, and Zig fetches Ghostty's package
dependencies unless `GHOSTTY_ZIG_SYSTEM_DIR` provides them. Both require network
access on a cold build.

To update, replace `src/` with the newer published crate, re-apply the build
changes above and confirm the Zig version Ghostty's `build.zig.zon` requires.
