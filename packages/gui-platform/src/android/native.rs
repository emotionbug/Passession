//! `crate::native` for Android.

use super::{call, call_async, call_await, object};
use crate::{
    action::*,
    audio::engine::AudioSink,
    paths::AppPaths,
    platform::{Capabilities, Completion, PickFilesOptions, PlatformState, Shared},
    NotificationRequest,
};
use makepad_widgets::{Cx, Event};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};

pub(crate) fn prepare_process(_cx: &mut Cx) -> anyhow::Result<()> {
    Ok(())
}

pub(crate) fn os_app_paths() -> AppPaths {
    // getFilesDir() is path_provider's application support directory on Android.
    let paths = call("paths", json!({})).ok().map(object).unwrap_or_default();
    let text = |key: &str| paths.get(key).and_then(Value::as_str).map(PathBuf::from);
    let cache = text("cache").unwrap_or_else(std::env::temp_dir);
    AppPaths {
        support_dir: text("files").unwrap_or_else(|| PathBuf::from("/data/data/sh.paseo.sessions/files")),
        temp_dir: cache.clone(),
        cache_dir: cache,
    }
}

pub(crate) fn capabilities() -> Capabilities {
    Capabilities {
        desktop_host: false,
        tray: false,
        editors: false,
        updater: false,
        directory_picker: false,
        save_dialog: false,
        push: true,
        qr_scan: true,
        share: true,
        idle: false,
    }
}

pub(crate) fn start_services(_cx: &mut Cx, _shared: &Arc<Shared>, state: &mut PlatformState) {
    // App audio uses AudioTrack on every supported Android version (API 24+),
    // with the original transient-exclusive audio focus.
    state.audio.set_sink(AudioSink {
        play: Arc::new(|request, audio| {
            let pcm: Vec<u8> = audio
                .samples
                .iter()
                .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
                .collect();
            if let Err(error) = super::audio_play(request, &pcm, audio.channels as i32, crate::audio::OUTPUT_RATE as i32) {
                post_result(request, Err(error));
            }
        }),
        cancel: Arc::new(super::audio_cancel),
    });
    // Links and taps that launched the activity are replayed now.
    let _ = call("startServices", json!({}));
}

pub(crate) fn handle_event(_cx: &mut Cx, _shared: &Arc<Shared>, _state: &mut PlatformState, event: &Event) {
    if let Event::Resume = event {
        crate::mobile::deliver(crate::mobile::MobileSignal::Resumed);
    }
}

pub(crate) fn report_error(_shared: &Arc<Shared>, message: &str) {
    post_event(PlatformEvent::HostError { message: message.to_owned() });
}

/// Back past the shell's own navigation: leave the app without finishing the
/// activity, so remote sessions are not stopped.
pub(crate) fn background_app(_cx: &mut Cx) {
    let _ = call("moveTaskToBack", json!({}));
}

/// The Java AudioTrack player owns audio focus.
pub(crate) fn acquire_audio_session() -> anyhow::Result<()> {
    Ok(())
}
pub(crate) fn release_audio_session() {}

fn none(_: Value) -> Result<PlatformValue, PlatformError> {
    Ok(PlatformValue::None)
}

pub(crate) fn open_external(shared: &Arc<Shared>, url: String, done: Completion) {
    shared.complete_blocking(done, move || call("openUrl", json!({"url": url})).and_then(none));
}

pub(crate) fn open_path(shared: &Arc<Shared>, path: PathBuf, done: Completion) {
    shared.complete_blocking(done, move || {
        let mime = mime_for(&path);
        call("openPath", json!({"path": path, "mime": mime})).and_then(none)
    });
}

pub(crate) fn reveal_path(_shared: &Arc<Shared>, _path: PathBuf, done: Completion) {
    done.fail(PlatformError::unsupported("Android has no file manager reveal; use share or open."));
}

fn mime_for(path: &std::path::Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    crate::paths::mime_for_name(&name).unwrap_or_else(|| "application/octet-stream".into())
}

fn files(value: Value) -> Result<PlatformValue, PlatformError> {
    let items = value.as_array().cloned().unwrap_or_default();
    Ok(PlatformValue::Files(
        items
            .into_iter()
            .filter_map(|item| {
                Some(PickedFile {
                    path: PathBuf::from(item.get("path")?.as_str()?),
                    name: item.get("name").and_then(Value::as_str).unwrap_or("file").to_owned(),
                    mime_type: item.get("mimeType").and_then(Value::as_str).map(str::to_owned),
                    size: item.get("size").and_then(Value::as_u64).unwrap_or(0),
                    temporary: true,
                })
            })
            .collect(),
    ))
}

/// ACTION_OPEN_DOCUMENT; selections are copied into a private cache directory.
pub(crate) fn pick_files(_shared: &Arc<Shared>, options: PickFilesOptions, done: Completion) {
    call_async("pickFiles", json!({"multiple": options.multiple, "imagesOnly": options.images_only}), done, files);
}

pub(crate) fn pick_directory(_shared: &Arc<Shared>, _title: Option<String>, done: Completion) {
    done.fail(PlatformError::unsupported("Mobile clients browse folders on the selected host."));
}

pub(crate) fn save_location(_shared: &Arc<Shared>, _suggested_name: String, done: Completion) {
    done.fail(PlatformError::unsupported("Mobile clients save through the share sheet."));
}

pub(crate) fn share_file(_shared: &Arc<Shared>, path: PathBuf, mime_type: String, name: String, done: Completion) {
    call_async("share", json!({"path": path, "mime": mime_type, "name": name}), done, none);
}

pub(crate) fn copy_text(shared: &Arc<Shared>, text: String, done: Completion) {
    shared.complete_blocking(done, move || call("copyText", json!({"text": text})).and_then(none));
}

pub(crate) fn read_clipboard(shared: &Arc<Shared>, images_only: bool, done: Completion) {
    shared.complete_blocking(done, move || {
        let directory = crate::paths::transfer_staging_file("clipboard")?;
        let reply = object(call("readClipboard", json!({"imagesOnly": images_only, "target": directory}))?);
        if let Some(path) = reply.get("imagePath").and_then(Value::as_str) {
            let bytes = std::fs::read(path)?;
            let _ = crate::paths::remove_transfer_staging(std::path::Path::new(path));
            let mime = crate::image::sniff_image_mime(&bytes)
                .map(str::to_owned)
                .ok_or_else(|| PlatformError::invalid("The clipboard image is not PNG, JPEG, GIF or WebP."))?;
            return Ok(PlatformValue::Clipboard(ClipboardContent {
                images: vec![ClipboardImage { bytes: Arc::new(bytes), mime_type: mime }],
                text: None,
            }));
        }
        let _ = crate::paths::remove_transfer_staging(&directory);
        if images_only {
            return Err(PlatformError::new("no_image", "No image in the current clipboard."));
        }
        Ok(PlatformValue::Clipboard(ClipboardContent {
            images: Vec::new(),
            text: reply.get("text").and_then(Value::as_str).map(str::to_owned),
        }))
    });
}

pub(crate) fn copy_image(shared: &Arc<Shared>, bytes: Vec<u8>, mime_type: String, done: Completion) {
    shared.complete_blocking(done, move || {
        let extension = match crate::image::sniff_image_mime(&bytes) {
            Some("image/png") => "png",
            Some("image/jpeg") => "jpg",
            Some("image/gif") => "gif",
            Some("image/webp") => "webp",
            _ => return Err(PlatformError::invalid(format!("Unsupported clipboard image type {mime_type}."))),
        };
        let file = crate::paths::transfer_staging_file(&format!("clipboard.{extension}"))?;
        std::fs::write(&file, &bytes)?;
        call("copyImage", json!({"path": file, "mime": mime_type})).and_then(none)
    });
}

/// Locally presented notification on the "default" channel.
pub(crate) fn notify(shared: &Arc<Shared>, request: NotificationRequest, done: Completion) {
    let payload = json!({"id": request.event_key.clone().unwrap_or_else(|| request.id.to_string()), "data": request.data});
    shared.complete_blocking(done, move || {
        call(
            "notify",
            json!({
                "id": request.id,
                "title": request.title,
                "body": request.body,
                "payload": payload.to_string(),
                "sound": request.sound.unwrap_or(true),
            }),
        )
        .and_then(none)
    });
}

fn permission(value: Value) -> Result<PlatformValue, PlatformError> {
    Ok(PlatformValue::Permission(parse_permission(&value)))
}

fn parse_permission(value: &Value) -> PermissionState {
    match value.as_str() {
        Some("granted") => PermissionState::Granted,
        Some("denied") => PermissionState::Denied,
        _ => PermissionState::NotDetermined,
    }
}

pub(crate) fn request_notification_permission(_shared: &Arc<Shared>, done: Completion) {
    call_async("permission", json!({"permission": "notifications", "request": true}), done, permission);
}

pub(crate) fn notification_permission_status(_shared: &Arc<Shared>, done: Completion) {
    call_async("permission", json!({"permission": "notifications", "request": false}), done, permission);
}

pub(crate) fn camera_permission(_shared: &Arc<Shared>, request: bool, done: Completion) {
    call_async("permission", json!({"permission": "camera", "request": request}), done, permission);
}

/// Launcher badges (the vendor broadcast app_badge_plus used; launchers
/// without numeric badges ignore it).
pub(crate) fn set_badge_count(shared: &Arc<Shared>, count: u32, done: Completion) {
    shared.complete_blocking(done, move || call("setBadge", json!({"count": count})).and_then(none));
}

pub(crate) fn idle_milliseconds(_shared: &Arc<Shared>, done: Completion) {
    done.fail(PlatformError::new("idle_unsupported", "Mobile clients report app activity only."));
}

pub(crate) fn system_appearance() -> (bool, String, crate::appearance::TextScale) {
    let value = call("appearance", json!({})).unwrap_or(Value::Null);
    let dark = value.get("dark").and_then(Value::as_bool).unwrap_or(false);
    let language = value.get("language").and_then(Value::as_str).unwrap_or("en").to_owned();
    let text_scale = value.get("textScaleDp")
        .and_then(|value| serde_json::from_value::<[f64; 10]>(value.clone()).ok())
        .and_then(crate::appearance::TextScale::checked)
        .unwrap_or_default();
    (dark, language, text_scale)
}

pub(crate) fn set_system_bars(dark: bool, background: u32) {
    if let Err(error) = call("setSystemBars", json!({"dark": dark, "background": background})) {
        post_event(PlatformEvent::HostError { message: error.message });
    }
}


pub(crate) fn insets_from_json(value: &Value) -> SafeInsets {
    let number = |key: &str| value.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    SafeInsets {
        top: number("top"),
        bottom: number("bottom"),
        left: number("left"),
        right: number("right"),
        keyboard: number("keyboard"),
        keyboard_visible: value.get("keyboardVisible").and_then(Value::as_bool).unwrap_or(false),
    }
}

// ----- push -----

/// Firebase is optional: builds without `google-services.json` cannot issue tokens.
pub(crate) async fn push_provisioned() -> bool {
    matches!(call_await("pushProvisioned", json!({})).await, Ok(Value::Bool(true)))
}

pub(crate) async fn notification_permission(request: bool) -> Result<PermissionState, PlatformError> {
    let value = call_await("permission", json!({"permission": "notifications", "request": request})).await?;
    Ok(parse_permission(&value))
}

/// The FCM registration token (Firebase Messaging 24 `getToken`).
pub(crate) async fn device_push_token() -> Result<String, PlatformError> {
    Ok(call_await("pushToken", json!({})).await?.as_str().unwrap_or("").to_owned())
}

/// Android FCM tokens are always production.
pub(crate) fn push_environment() -> Result<String, PlatformError> {
    Ok("production".into())
}

/// Errors recorded by the background messaging service while no UI ran.
pub(crate) fn take_background_push_error() -> Option<String> {
    call("takeBackgroundError", json!({})).ok().and_then(|v| v.as_str().map(str::to_owned))
}

/// Android font discovery: `AFontMatcher` (API 29+, dlopened from
/// `libandroid.so`), else the `fonts.xml` families the Java bridge parses.
pub(crate) mod fonts {
    use super::call;
    use crate::fonts::{bcp47, finish_face, resolve, Backend, Family, FileCache, FontFace, FontRequest, Role, SystemFonts, HANGUL_PROBE, LATIN_PROBE};
    use serde_json::{json, Value};
    use std::{
        ffi::{c_char, c_void, CStr, CString},
        sync::LazyLock,
    };

    /// Locale list for Hangul lookups.
    const KOREAN: &str = "ko-KR";

    struct Matcher {
        create: unsafe extern "C" fn() -> *mut c_void,
        destroy: unsafe extern "C" fn(*mut c_void),
        set_style: unsafe extern "C" fn(*mut c_void, u16, bool),
        set_locales: unsafe extern "C" fn(*mut c_void, *const c_char),
        find: unsafe extern "C" fn(*const c_void, *const c_char, *const u16, u32, *mut u32) -> *mut c_void,
        close: unsafe extern "C" fn(*mut c_void),
        file_path: unsafe extern "C" fn(*const c_void) -> *const c_char,
        collection_index: unsafe extern "C" fn(*const c_void) -> usize,
        axis_count: unsafe extern "C" fn(*const c_void) -> usize,
        axis_tag: unsafe extern "C" fn(*const c_void, u32) -> u32,
        axis_value: unsafe extern "C" fn(*const c_void, u32) -> f32,
    }

    impl Matcher {
        /// `None` below API 29, where libandroid does not export the NDK font API.
        fn load() -> Option<Self> {
            unsafe {
                let library = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
                if library.is_null() {
                    return None;
                }
                macro_rules! symbol {
                    ($name:literal) => {{
                        let symbol = libc::dlsym(library, $name.as_ptr());
                        if symbol.is_null() {
                            return None;
                        }
                        std::mem::transmute::<*mut c_void, _>(symbol)
                    }};
                }
                // libandroid stays loaded for the process (it is already mapped by the runtime).
                Some(Self {
                    create: symbol!(c"AFontMatcher_create"),
                    destroy: symbol!(c"AFontMatcher_destroy"),
                    set_style: symbol!(c"AFontMatcher_setStyle"),
                    set_locales: symbol!(c"AFontMatcher_setLocales"),
                    find: symbol!(c"AFontMatcher_match"),
                    close: symbol!(c"AFont_close"),
                    file_path: symbol!(c"AFont_getFontFilePath"),
                    collection_index: symbol!(c"AFont_getCollectionIndex"),
                    axis_count: symbol!(c"AFont_getAxisCount"),
                    axis_tag: symbol!(c"AFont_getAxisTag"),
                    axis_value: symbol!(c"AFont_getAxisValue"),
                })
            }
        }

        /// The system's choice for `family` rendering `text`: file, index and axes.
        fn find(&self, family: &str, text: char, role: Role, locales: &str) -> Option<Candidate> {
            let family = CString::new(family).ok()?;
            let locales = CString::new(locales).ok()?;
            let mut units = [0u16; 2];
            let units = text.encode_utf16(&mut units);
            unsafe {
                let matcher = (self.create)();
                if matcher.is_null() {
                    return None;
                }
                (self.set_style)(matcher, role.weight(), role.italic());
                (self.set_locales)(matcher, locales.as_ptr());
                let mut run = 0u32;
                let font = (self.find)(matcher, family.as_ptr(), units.as_ptr(), units.len() as u32, &mut run);
                (self.destroy)(matcher);
                if font.is_null() {
                    return None;
                }
                let path = (self.file_path)(font);
                let candidate = (!path.is_null()).then(|| Candidate {
                    path: CStr::from_ptr(path).to_string_lossy().into_owned(),
                    index: (self.collection_index)(font) as u32,
                    axes: (0..(self.axis_count)(font) as u32)
                        .map(|axis| ((self.axis_tag)(font, axis).to_be_bytes(), (self.axis_value)(font, axis)))
                        .collect(),
                });
                (self.close)(font);
                candidate
            }
        }
    }

    static MATCHER: LazyLock<Option<Matcher>> = LazyLock::new(Matcher::load);

    #[derive(Clone, Debug, PartialEq)]
    struct Candidate {
        path: String,
        index: u32,
        axes: Vec<([u8; 4], f32)>,
    }

    /// Faces of one `fonts.xml` family (Java `systemFonts`).
    fn xml_faces(value: &Value) -> Vec<(Candidate, u16, bool)> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|font| {
                let axes = font
                    .get("axes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|axis| {
                        let tag: [u8; 4] = axis.get("tag")?.as_str()?.as_bytes().try_into().ok()?;
                        Some((tag, axis.get("value")?.as_f64()? as f32))
                    })
                    .collect();
                let candidate = Candidate {
                    path: font.get("path")?.as_str()?.to_owned(),
                    index: font.get("index").and_then(Value::as_u64).unwrap_or(0) as u32,
                    axes,
                };
                let weight = font.get("weight").and_then(Value::as_u64).unwrap_or(400) as u16;
                Some((candidate, weight, font.get("italic").and_then(Value::as_bool).unwrap_or(false)))
            })
            .collect()
    }

    /// The face closest to `role`: matching slant first, then nearest weight.
    fn closest(faces: &[(Candidate, u16, bool)], role: Role) -> Option<Candidate> {
        faces
            .iter()
            .min_by_key(|(_, weight, italic)| (*italic != role.italic(), weight.abs_diff(role.weight())))
            .map(|(candidate, _, _)| candidate.clone())
    }

    /// File stem up to its style suffix: `/system/fonts/Roboto-Regular.ttf` → `Roboto`.
    fn family_from_path(path: &str) -> String {
        let stem = path.rsplit('/').next().unwrap_or(path);
        stem.split(['-', '[', '.']).next().unwrap_or(stem).to_owned()
    }

    struct Android {
        files: FileCache,
        locales: String,
        xml: Option<Value>,
    }

    impl Android {
        fn xml(&mut self) -> &Value {
            self.xml.get_or_insert_with(|| call("systemFonts", json!({})).unwrap_or(Value::Null))
        }

        /// A named `fonts.xml` family (aliases followed once) at `role`.
        fn xml_family(&mut self, name: &str, role: Role) -> Option<Candidate> {
            let xml = self.xml();
            let families = xml.get("families")?;
            if let Some(faces) = families.get(name) {
                return closest(&xml_faces(faces), role);
            }
            let alias = xml.get("aliases")?.get(name)?;
            let faces = xml_faces(families.get(alias.get("to")?.as_str()?)?);
            match alias.get("weight").and_then(Value::as_u64).filter(|weight| *weight > 0) {
                // A weight alias (`sans-serif-medium`) picks that weight for upright roles.
                Some(weight) => faces
                    .iter()
                    .min_by_key(|(_, face_weight, italic)| (*italic != role.italic(), face_weight.abs_diff(weight as u16)))
                    .map(|(candidate, _, _)| candidate.clone()),
                None => closest(&faces, role),
            }
        }

        fn load(&mut self, candidate: &Candidate, role: Role, probe: char) -> Option<FontFace> {
            let data = self.files.read(&candidate.path)?;
            finish_face(data, candidate.index, &candidate.axes, role, probe, format!("{}#{}", candidate.path, candidate.index))
        }

        /// `family` through the matcher (or `fonts.xml`). Named families that
        /// fall back to the default sans-serif face are reported as missing.
        fn lookup(&mut self, family: &str, role: Role, exact: bool) -> Option<Candidate> {
            let locales = self.locales.clone();
            match &*MATCHER {
                Some(matcher) => {
                    let found = matcher.find(family, LATIN_PROBE, role, &locales)?;
                    // Installed aliases can intentionally select the default face.
                    // Only an undeclared family resolving there is substitution.
                    if exact
                        && matcher.find("sans-serif", LATIN_PROBE, role, &locales).as_ref() == Some(&found)
                        && self.xml_family(family, role).is_none()
                    {
                        return None;
                    }
                    Some(found)
                }
                None => self.xml_family(family, role),
            }
        }
    }

    impl Backend for Android {
        fn face(&mut self, family: Family<'_>, role: Role) -> Option<(FontFace, String)> {
            let (name, exact) = match family {
                Family::SystemUi => ("sans-serif", false),
                Family::SystemMono => ("monospace", false),
                Family::Named(name) => (name, !name.eq_ignore_ascii_case("sans-serif")),
            };
            let candidate = self.lookup(name, role, exact)?;
            let face = self.load(&candidate, role, LATIN_PROBE)?;
            let reported = if exact { name.to_owned() } else { family_from_path(&candidate.path) };
            Some((face, reported))
        }

        fn hangul(&mut self, _ui_family: Option<&str>, role: Role) -> Option<FontFace> {
            let candidates = match &*MATCHER {
                Some(matcher) => matcher.find("sans-serif", HANGUL_PROBE, role, KOREAN).into_iter().collect::<Vec<_>>(),
                None => {
                    let faces = xml_faces(self.xml().get("hangul").unwrap_or(&Value::Null));
                    let mut ordered = faces.clone();
                    ordered.sort_by_key(|(_, weight, italic)| (*italic != role.italic(), weight.abs_diff(role.weight())));
                    ordered.into_iter().map(|(candidate, _, _)| candidate).collect()
                }
            };
            candidates.iter().find_map(|candidate| self.load(candidate, role, HANGUL_PROBE))
        }
    }

    /// API 29+: `AFontMatcher` for `sans-serif` / `monospace` / named families
    /// with the app locale, and for 한 with `ko-KR`. API 24–28: the families and
    /// Korean fallback faces declared in `/system/etc/fonts.xml`.
    pub(crate) fn system_fonts(request: &FontRequest) -> SystemFonts {
        let locale = bcp47(&request.locale);
        let locales = if locale.is_empty() { KOREAN.to_owned() } else { format!("{locale},{KOREAN}") };
        resolve(&mut Android { files: FileCache::default(), locales, xml: None }, request)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn declared_alias_of_default_font_is_not_missing() {
            let Some(matcher) = &*MATCHER else { return };
            let mut backend = Android { files: FileCache::default(), locales: "en-US".into(), xml: None };
            let aliases: Vec<String> = backend.xml().get("aliases").and_then(Value::as_object)
                .map(|aliases| aliases.keys().cloned().collect()).unwrap_or_default();
            let default = matcher.find("sans-serif", LATIN_PROBE, Role::Regular, &backend.locales);
            assert!(default.is_some());
            let alias = aliases.into_iter().find(|name| {
                backend.xml_family(name, Role::Regular).is_some()
                    && matcher.find(name, LATIN_PROBE, Role::Regular, &backend.locales) == default
            });
            let Some(alias) = alias else {
                eprintln!("skipped: no installed alias selects the default face");
                return;
            };
            assert!(backend.lookup(&alias, Role::Regular, true).is_some());
            assert!(backend.lookup("No Such Family Paseo 7f3a", Role::Regular, true).is_none());
        }
    }
}
