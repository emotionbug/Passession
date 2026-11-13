//! Desktop and mobile OS integration for the native Passession client.
//!
//! See `platform-api.md` in the migration state directory for the contract.
//! Android and iOS are remote-only: no daemon, desktop-host, tray or updater
//! code is compiled for them.

pub mod action;
pub mod audio;
pub mod deep_link;
pub mod image;
pub mod legacy_prefs;
pub mod notification_policy;
pub mod paths;
mod facade;
mod platform;
#[cfg(any(target_os = "android", target_os = "ios"))]
pub(crate) mod prefs;
pub mod qr;

pub use facade::{appearance, notifications};

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

#[cfg(target_os = "ios")]
pub(crate) mod ios;

/// The OS backend selected for this target. Every backend implements the same
/// set of functions; there is no runtime fallback between them.
pub(crate) mod native {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub(crate) use crate::desktop::native::*;
    #[cfg(target_os = "android")]
    pub(crate) use crate::android::native::*;
    #[cfg(target_os = "ios")]
    pub(crate) use crate::ios::native::*;
}

pub use action::{
    CloseDecision, ClipboardContent, ClipboardImage, EditorTarget, PermissionState, PickedFile, PlatformAction,
    PlatformError, PlatformEvent, PlatformRequestId, PlatformRoute, PlatformValue, QuitReason, SafeInsets,
};
pub use notification_policy::{AttentionView, NotificationRequest};
pub use platform::{
    dropped_file_paths, platform, Capabilities, OpenEditorRequest, PickFilesOptions, Platform, PlatformConfig,
    PlatformHandle, WindowGeometry,
};

/// Desktop single-instance handling; mobile processes are always primary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bootstrap {
    Primary,
    /// Another instance received this launch's arguments; exit with this code.
    Forwarded { exit_code: i32 },
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
