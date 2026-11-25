//! The web client's WebAssembly module: the native GUI built for `wasm32-wasip1`.
//!
//! Linking `paseo_gui` exports Makepad's entry points (from the GUI's `app_main!`) and the
//! `paseo_*` browser callbacks of `paseo-browser`; the page's `boot.js` instantiates it. Native
//! builds compile this crate empty.
#![cfg(target_arch = "wasm32")]

extern crate paseo_gui;
