//! Paseo vendoring change (see VENDORED.md): NDK libraries newer than the
//! app's minimum SDK (libaaudio API 26, libamidi API 29) are resolved at
//! runtime instead of linked, so the library links against the API 24 sysroot
//! and loads on every supported device. Each wrapper keeps the original
//! `extern "C"` signature; when the library or a symbol is absent the call
//! returns the given error value (or does nothing for `()`).

macro_rules! dynamic_ndk_library {
    (
        $library:literal, $api:ident, $loader:ident;
        $( fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)? = $fallback:expr; )*
    ) => {
        #[allow(non_snake_case)]
        struct $api {
            _module: crate::module_loader::ModuleLoader,
            $( $name: unsafe extern "C" fn($($ty),*) $(-> $ret)?, )*
        }
        // The module handle and function pointers are process-global and immutable.
        unsafe impl Send for $api {}
        unsafe impl Sync for $api {}

        fn $loader() -> Option<&'static $api> {
            static API: std::sync::OnceLock<Option<$api>> = std::sync::OnceLock::new();
            API.get_or_init(|| {
                let module = crate::module_loader::ModuleLoader::load($library).ok()?;
                Some($api {
                    $( $name: module.get_symbol(stringify!($name)).ok()?, )*
                    _module: module,
                })
            })
            .as_ref()
        }

        /// True when the library and every symbol resolved on this device.
        #[allow(dead_code)]
        pub fn available() -> bool {
            $loader().is_some()
        }

        $(
            #[allow(non_snake_case, clippy::missing_safety_doc)]
            pub unsafe fn $name($($arg: $ty),*) $(-> $ret)? {
                match $loader() {
                    Some(api) => (api.$name)($($arg),*),
                    None => $fallback,
                }
            }
        )*
    };
}

pub(crate) use dynamic_ndk_library;
