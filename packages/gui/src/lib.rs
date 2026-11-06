//! Paseo Sessions native client (Makepad). See `ui-api.md` for the shell/feature boundary.

pub use makepad_widgets;

pub mod app;
pub mod common;
pub mod features;
pub mod shell;
pub mod theme;
#[cfg(feature = "desktop-smoke")]
mod smoke;
mod theme_makepad;
