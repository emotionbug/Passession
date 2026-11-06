//! Small Win32 helpers shared by path resolution and integrations.

use std::{os::windows::ffi::OsStringExt as _, path::PathBuf};
use windows::{
    core::{HSTRING, PCWSTR},
    Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_RoamingAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT},
    },
};

/// FOLDERID_RoamingAppData (what path_provider_windows used).
pub(crate) fn roaming_app_data() -> Option<PathBuf> {
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, None).ok()?;
        let wide = path.as_wide();
        let result = PathBuf::from(std::ffi::OsString::from_wide(wide));
        CoTaskMemFree(Some(path.0 as *const _));
        Some(result)
    }
}

/// A NUL-terminated UTF-16 buffer that outlives the PCWSTR borrowed from it.
pub(crate) struct Wide(pub Vec<u16>);

impl Wide {
    pub fn new(text: impl AsRef<std::ffi::OsStr>) -> Self {
        use std::os::windows::ffi::OsStrExt as _;
        let mut buffer: Vec<u16> = text.as_ref().encode_wide().collect();
        buffer.push(0);
        Self(buffer)
    }
    pub fn pcwstr(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }
}

pub(crate) fn hstring(text: &str) -> HSTRING {
    HSTRING::from(text)
}
