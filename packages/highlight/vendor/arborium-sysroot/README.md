# arborium-sysroot

WASM sysroot provider for arborium grammar plugins.

## Purpose

This crate serves two functions:

1. **Build-time**: Provides the wasm-sysroot path to dependent crates via the
   `DEP_ARBORIUM_SYSROOT_PATH` environment variable set by build.rs

2. **Runtime**: Includes WASM allocator implementations for browser compatibility

This is an internal crate used by arborium's plugin build system.
---

Part of the [arborium](https://github.com/bearcove/arborium) project. See [arborium.bearcove.eu](https://arborium.bearcove.eu) for more information.
