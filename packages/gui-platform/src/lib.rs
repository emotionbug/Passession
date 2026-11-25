//! Desktop, mobile and browser integration for the Passession client.
//!
//! Mobile and browser clients do not run a local daemon or updater.

pub mod action;
pub mod audio;
pub mod deep_link;
mod facade;
mod fonts;
pub mod image;
pub mod notification_policy;
pub mod paths;
mod platform;
#[cfg(any(target_os = "android", target_os = "ios"))]
pub(crate) mod prefs;
pub mod qr;
#[cfg(not(target_arch = "wasm32"))]
mod tailnet_keys;
#[cfg(target_arch = "wasm32")]
mod web;
pub mod webview;

pub use facade::{appearance, notifications};
#[cfg(not(target_arch = "wasm32"))]
pub use tailnet_keys::tailnet_key_store;

/// Desktop host internals; `available()` / `invoke()` are its public facade.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub mod desktop;

/// Clients without a local host only expose its unavailable facade.
#[cfg(any(target_os = "android", target_os = "ios", target_arch = "wasm32"))]
pub mod desktop {
    pub use crate::facade::desktop::*;
}

#[cfg(any(target_os = "android", target_os = "ios"))]
pub(crate) mod mobile;

#[cfg(target_os = "android")]
pub(crate) mod android;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) mod apple;
/// CoreText font discovery shared by macOS and iOS.
#[cfg(any(target_os = "macos", target_os = "ios"))]
#[path = "apple/fonts.rs"]
pub(crate) mod apple_fonts;

#[cfg(target_os = "ios")]
pub(crate) mod ios;

/// The backend selected at compile time.
pub(crate) mod native {
    #[cfg(target_os = "android")]
    pub(crate) use crate::android::native::*;
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    pub(crate) use crate::desktop::native::*;
    #[cfg(target_os = "ios")]
    pub(crate) use crate::ios::native::*;
    #[cfg(target_arch = "wasm32")]
    pub(crate) use crate::web::*;
}

#[cfg(target_arch = "wasm32")]
pub use action::PickedFileBytes;
pub use action::{
    ClipboardContent, ClipboardImage, EditorTarget, ImageShareError, PermissionSettings,
    PermissionState, PickedFile, PlatformAction, PlatformError, PlatformEvent, PlatformRequestId,
    PlatformRoute, PlatformValue, QuitReason, SafeInsets, SharedImages, TailnetControl,
    TailnetPresence, TailnetPresenceState,
};
pub use notification_policy::{AttentionView, NotificationRequest};
pub use platform::{
    Capabilities, OpenEditorRequest, PickFilesOptions, Platform, PlatformConfig, PlatformHandle,
    WindowGeometry, dropped_file_paths, platform,
};

/// Desktop single-instance handling; mobile processes are always primary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bootstrap {
    Primary,
    /// Another instance received this launch's arguments; exit with this code.
    Forwarded {
        exit_code: i32,
    },
    /// The browser owns the lifetime of each tab.
    #[cfg(target_arch = "wasm32")]
    Unavailable,
}

/// Call before any window is created. A secondary desktop launch forwards its
/// arguments (deep link, project path) to the running instance and must exit.
pub fn bootstrap_process(argv: &[String]) -> Bootstrap {
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    return desktop::single_instance::bootstrap(argv);
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = argv;
        Bootstrap::Primary
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = argv;
        Bootstrap::Unavailable
    }
}

/// Registers the platform widgets (`QrScanView`). Call from the shell's
/// `live_register` before the widgets that use them.
pub fn live_design(cx: &mut makepad_widgets::Cx) {
    qr::live_design(cx);
}
