//! Desktop and mobile OS integration for the native Passession client.
//!
//! See `platform-api.md` in the migration state directory for the contract.
//! Android and iOS are remote-only; no local daemon or updater code is linked.

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
mod tailnet_keys;

pub use facade::{appearance, notifications};
pub use tailnet_keys::tailnet_key_store;

/// Desktop host internals; `available()` / `invoke()` are its public facade.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod desktop;

/// Mobile clients never manage a local daemon.
#[cfg(any(target_os = "android", target_os = "ios"))]
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

/// The OS backend selected for this target. Every backend implements the same
/// set of functions; there is no runtime fallback between them.
pub(crate) mod native {
    #[cfg(target_os = "android")]
    pub(crate) use crate::android::native::*;
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub(crate) use crate::desktop::native::*;
    #[cfg(target_os = "ios")]
    pub(crate) use crate::ios::native::*;
}

pub use action::{
    ClipboardContent, ClipboardImage, EditorTarget, PermissionSettings, PermissionState,
    PickedFile, PlatformAction, PlatformError, PlatformEvent, PlatformRequestId, PlatformRoute,
    PlatformValue, QuitReason, SafeInsets, TailnetControl, TailnetPresence, TailnetPresenceState,
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
}

/// Call before any window is created. A secondary desktop launch forwards its
/// arguments (deep link, project path) to the running instance and must exit.
pub fn bootstrap_process(argv: &[String]) -> Bootstrap {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    return desktop::single_instance::bootstrap(argv);
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = argv;
        Bootstrap::Primary
    }
}

/// Registers the platform widgets (`QrScanView`). Call from the shell's
/// `live_register` before the widgets that use them.
pub fn live_design(cx: &mut makepad_widgets::Cx) {
    qr::live_design(cx);
}
