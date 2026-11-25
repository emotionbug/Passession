# Vendored arborium crates

`arborium/` and `arborium-sysroot/` are the published crates `arborium 2.18.2`
(`arborium-2.18.2.crate`, SHA-256
`230e826fe3204178030e420751396c99431e4611fc67af63baf7767b4317bd75`) and
`arborium-sysroot 2.18.2` (`arborium-sysroot-2.18.2.crate`, SHA-256
`ddd9d120dd5df8b918cd96676a88f1233a66f10687d415b69f70b3396acddddf`) from
crates.io, extracted unchanged except for the lines listed below. The archives'
`Cargo.lock` and VCS metadata were dropped; the workspace lockfile resolves
dependencies. The root `Cargo.toml` applies them through `[patch.crates-io]`
and excludes this directory from the workspace members.

The published archives carry no license file. A path copy must carry its own
(`xtask/src/notices.rs`), so `LICENSE-MIT` (both crates) and `LICENSE-APACHE`
(`arborium`, licensed `MIT OR Apache-2.0`; `arborium-sysroot` is `MIT`) are the
upstream files at revision `45fae8adc0d4e62a42d68eda0454f606b3fff6aa`, the same
texts `packaging/cargo-notice-supplements.json` pins for the registry crate.

## WASI keeps its libc

Upstream treats every WebAssembly target as libc-less: on `target_family =
"wasm"` both crates define `malloc`, `calloc`, `realloc`, `free`, `fwrite` and
other C symbols for tree-sitter, and `arborium-sysroot` compiles C replacements.
The web client is built for `wasm32-wasip1`, whose standard library links
wasi-libc, so those definitions collide with it at link time (`duplicate symbol:
malloc`).

The shims are therefore limited to `target_os = "unknown"`
(`wasm32-unknown-unknown`, the case they exist for):

- `arborium/src/lib.rs`: `mod wasm` is `cfg(all(target_family = "wasm", target_os = "unknown"))`.
- `arborium/Cargo.toml`: the `dlmalloc` dependency uses the same condition.
- `arborium-sysroot/src/lib.rs`: `mod wasm` and its re-export use the same condition.
- `arborium-sysroot/build.rs`: the C replacements compile only when `CARGO_CFG_TARGET_OS`
  is `unknown`.

`arborium-tree-sitter` already selects its own WebAssembly runtime only for
`wasm32-unknown` targets and needs no change. Native targets are unaffected.
