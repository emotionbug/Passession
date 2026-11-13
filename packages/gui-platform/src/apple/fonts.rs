//! CoreText font discovery shared by macOS and iOS.
//!
//! System fonts are read through `CTFontCopyTable` and rebuilt into an sfnt in
//! memory, so neither the face index inside `.ttc` collections nor sandboxed
//! file access matters. Variable instances (SF at semibold, …) report their
//! coordinates through `CTFontCopyVariation`.

use crate::{
    apple::{ns_string, rust_string, Id},
    fonts::{bcp47, build_sfnt, finish_face, resolve, Backend, Family, FontFace, FontRequest, Role, SystemFonts, HANGUL_PROBE, LATIN_PROBE},
};
use objc2::{class, msg_send, rc::autoreleasepool};
use std::{collections::HashMap, ffi::c_void, sync::Arc};

type CFTypeRef = *const c_void;
type CFIndex = isize;

#[repr(C)]
#[derive(Clone, Copy)]
struct CFRange {
    location: CFIndex,
    length: CFIndex,
}

/// `kCTFontUIFontUserFixedPitch`.
const UI_FONT_USER_FIXED_PITCH: u32 = 1;
/// `kCTFontUIFontSystem`.
const UI_FONT_SYSTEM: u32 = 2;
/// `kCTFontItalicTrait`.
const ITALIC_TRAIT: u32 = 1 << 0;
/// `kCFNumberSInt64Type` / `kCFNumberFloat64Type`.
const NUMBER_SINT64: CFIndex = 4;
const NUMBER_FLOAT64: CFIndex = 6;
/// Point size of the probe fonts; tables and variations do not depend on it.
const SIZE: f64 = 14.0;

#[link(name = "CoreText", kind = "framework")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCTFontFamilyNameAttribute: CFTypeRef;
    static kCTFontTraitsAttribute: CFTypeRef;
    static kCTFontWeightTrait: CFTypeRef;
    static kCTFontSymbolicTrait: CFTypeRef;

    fn CTFontCreateUIFontForLanguage(ui_type: u32, size: f64, language: CFTypeRef) -> CFTypeRef;
    fn CTFontDescriptorCreateWithAttributes(attributes: CFTypeRef) -> CFTypeRef;
    fn CTFontCreateWithFontDescriptor(descriptor: CFTypeRef, size: f64, matrix: *const c_void) -> CFTypeRef;
    fn CTFontCreateCopyWithSymbolicTraits(font: CFTypeRef, size: f64, matrix: *const c_void, value: u32, mask: u32) -> CFTypeRef;
    fn CTFontCreateForString(font: CFTypeRef, string: CFTypeRef, range: CFRange) -> CFTypeRef;
    fn CTFontCopyFamilyName(font: CFTypeRef) -> CFTypeRef;
    fn CTFontCopyPostScriptName(font: CFTypeRef) -> CFTypeRef;
    fn CTFontCopyAvailableTables(font: CFTypeRef, options: u32) -> CFTypeRef;
    fn CTFontCopyTable(font: CFTypeRef, tag: u32, options: u32) -> CFTypeRef;
    fn CTFontCopyVariation(font: CFTypeRef) -> CFTypeRef;

    fn CFRetain(object: CFTypeRef) -> CFTypeRef;
    fn CFRelease(object: CFTypeRef);
    fn CFArrayGetCount(array: CFTypeRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, index: CFIndex) -> *const c_void;
    fn CFDataGetLength(data: CFTypeRef) -> CFIndex;
    fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    fn CFDictionaryGetCount(dictionary: CFTypeRef) -> CFIndex;
    fn CFDictionaryGetKeysAndValues(dictionary: CFTypeRef, keys: *mut *const c_void, values: *mut *const c_void);
    fn CFNumberGetValue(number: CFTypeRef, kind: CFIndex, value: *mut c_void) -> u8;
}

/// An owned (+1) CoreFoundation reference.
struct Cf(CFTypeRef);

impl Cf {
    fn new(object: CFTypeRef) -> Option<Self> {
        (!object.is_null()).then(|| Self(object))
    }

    /// Takes a +1 reference to an autoreleased object.
    fn retain(object: Id) -> Option<Self> {
        (!object.is_null()).then(|| Self(unsafe { CFRetain(object as CFTypeRef) }))
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

fn cf_string(object: &Cf) -> Option<String> {
    rust_string(object.0 as Id)
}

fn data_bytes(data: &Cf) -> Vec<u8> {
    unsafe {
        let length = CFDataGetLength(data.0);
        let bytes = CFDataGetBytePtr(data.0);
        if bytes.is_null() || length <= 0 {
            return Vec::new();
        }
        std::slice::from_raw_parts(bytes, length as usize).to_vec()
    }
}

fn number_i64(number: *const c_void) -> Option<i64> {
    let mut value = 0i64;
    (unsafe { CFNumberGetValue(number, NUMBER_SINT64, (&raw mut value).cast()) } != 0).then_some(value)
}

fn number_f64(number: *const c_void) -> Option<f64> {
    let mut value = 0f64;
    (unsafe { CFNumberGetValue(number, NUMBER_FLOAT64, (&raw mut value).cast()) } != 0).then_some(value)
}

/// `NSFontWeight` / `UIFontWeight` and the `kCTFontWeightTrait` scale.
fn weight_trait(role: Role) -> f64 {
    if role.weight() >= 600 { 0.3 } else { 0.0 }
}

/// Applies the italic trait; fonts without an italic keep their upright face.
fn with_slant(font: Cf, role: Role) -> Cf {
    if !role.italic() {
        return font;
    }
    let italic = unsafe { CTFontCreateCopyWithSymbolicTraits(font.0, 0.0, std::ptr::null(), ITALIC_TRAIT, ITALIC_TRAIT) };
    Cf::new(italic).unwrap_or(font)
}

/// `[NSFont|UIFont systemFontOfSize:weight:]` (toll-free bridged to `CTFontRef`).
fn system_ui_font(role: Role) -> Option<Cf> {
    #[cfg(target_os = "macos")]
    let font: Id = unsafe { msg_send![class!(NSFont), systemFontOfSize: SIZE, weight: weight_trait(role)] };
    #[cfg(target_os = "ios")]
    let font: Id = unsafe { msg_send![class!(UIFont), systemFontOfSize: SIZE, weight: weight_trait(role)] };
    Some(with_slant(Cf::retain(font)?, role))
}

/// An installed family by name; `None` when CoreText substitutes another family.
fn named_font(name: &str, role: Role) -> Option<Cf> {
    unsafe {
        let trait_keys: [Id; 2] = [kCTFontWeightTrait as Id, kCTFontSymbolicTrait as Id];
        let symbolic: u32 = if role.italic() { ITALIC_TRAIT } else { 0 };
        let trait_values: [Id; 2] =
            [msg_send![class!(NSNumber), numberWithDouble: weight_trait(role)], msg_send![class!(NSNumber), numberWithUnsignedInt: symbolic]];
        let traits: Id = msg_send![class!(NSDictionary), dictionaryWithObjects: trait_values.as_ptr(), forKeys: trait_keys.as_ptr(), count: 2usize];
        let keys: [Id; 2] = [kCTFontFamilyNameAttribute as Id, kCTFontTraitsAttribute as Id];
        let values: [Id; 2] = [ns_string(name), traits];
        let attributes: Id = msg_send![class!(NSDictionary), dictionaryWithObjects: values.as_ptr(), forKeys: keys.as_ptr(), count: 2usize];
        let descriptor = Cf::new(CTFontDescriptorCreateWithAttributes(attributes as CFTypeRef))?;
        let font = Cf::new(CTFontCreateWithFontDescriptor(descriptor.0, SIZE, std::ptr::null()))?;
        let family = Cf::new(CTFontCopyFamilyName(font.0)).and_then(|family| cf_string(&family))?;
        family.eq_ignore_ascii_case(name).then_some(font)
    }
}

fn family_name(font: &Cf) -> Option<String> {
    Cf::new(unsafe { CTFontCopyFamilyName(font.0) }).and_then(|name| cf_string(&name))
}

struct CoreText {
    language: String,
    /// Rebuilt sfnts keyed by table list + `head` bytes, so instances of one
    /// variable font (SF regular/semibold) share their bytes.
    sfnts: HashMap<(Vec<u32>, Vec<u8>), Arc<Vec<u8>>>,
}

impl CoreText {
    fn sfnt(&mut self, font: &Cf) -> Option<Arc<Vec<u8>>> {
        let tables = Cf::new(unsafe { CTFontCopyAvailableTables(font.0, 0) })?;
        // The array stores CTFontTableTag values directly, not CFNumbers.
        let tags: Vec<u32> = (0..unsafe { CFArrayGetCount(tables.0) })
            .map(|index| unsafe { CFArrayGetValueAtIndex(tables.0, index) } as usize as u32)
            .collect();
        let table = |tag: u32| Cf::new(unsafe { CTFontCopyTable(font.0, tag, 0) }).map(|data| data_bytes(&data));
        let head = table(u32::from_be_bytes(*b"head"))?;
        let key = (tags.clone(), head);
        if let Some(data) = self.sfnts.get(&key) {
            return Some(data.clone());
        }
        let tables: Vec<([u8; 4], Vec<u8>)> = tags.iter().filter_map(|tag| Some((tag.to_be_bytes(), table(*tag)?))).collect();
        let data = Arc::new(build_sfnt(&tables));
        self.sfnts.insert(key, data.clone());
        Some(data)
    }

    fn variations(font: &Cf) -> Vec<([u8; 4], f32)> {
        let Some(variation) = Cf::new(unsafe { CTFontCopyVariation(font.0) }) else { return Vec::new() };
        let count = unsafe { CFDictionaryGetCount(variation.0) }.max(0) as usize;
        let mut keys = vec![std::ptr::null(); count];
        let mut values = vec![std::ptr::null(); count];
        unsafe { CFDictionaryGetKeysAndValues(variation.0, keys.as_mut_ptr(), values.as_mut_ptr()) };
        keys.into_iter()
            .zip(values)
            .filter_map(|(key, value)| Some(((number_i64(key)? as u32).to_be_bytes(), number_f64(value)? as f32)))
            .collect()
    }

    fn load(&mut self, font: &Cf, role: Role, probe: char) -> Option<FontFace> {
        let data = self.sfnt(font)?;
        let name = Cf::new(unsafe { CTFontCopyPostScriptName(font.0) }).and_then(|name| cf_string(&name)).unwrap_or_default();
        finish_face(data, 0, &Self::variations(font), role, probe, format!("CoreText {name}"))
    }

    fn font(&self, family: Family<'_>, role: Role) -> Option<Cf> {
        match family {
            Family::SystemUi => system_ui_font(role),
            Family::SystemMono => {
                let language = ns_string(&self.language);
                Cf::new(unsafe { CTFontCreateUIFontForLanguage(UI_FONT_USER_FIXED_PITCH, SIZE, language as CFTypeRef) })
            }
            Family::Named(name) => named_font(name, role),
        }
    }
}

impl Backend for CoreText {
    fn face(&mut self, family: Family<'_>, role: Role) -> Option<(FontFace, String)> {
        let font = self.font(family, role)?;
        let name = family_name(&font).unwrap_or_default();
        Some((self.load(&font, role, LATIN_PROBE)?, name))
    }

    /// CoreText's cascade for 한 from the UI font (Apple SD Gothic Neo).
    fn hangul(&mut self, ui_family: Option<&str>, role: Role) -> Option<FontFace> {
        let base = ui_family
            .and_then(|name| named_font(name, role))
            .or_else(|| system_ui_font(role))
            .or_else(|| {
                let language = ns_string(&self.language);
                Cf::new(unsafe { CTFontCreateUIFontForLanguage(UI_FONT_SYSTEM, SIZE, language as CFTypeRef) })
            })?;
        let text = ns_string(&HANGUL_PROBE.to_string());
        let font = Cf::new(unsafe { CTFontCreateForString(base.0, text as CFTypeRef, CFRange { location: 0, length: 1 }) })?;
        self.load(&font, role, HANGUL_PROBE)
    }
}

/// CoreText resolution: `systemFontOfSize:weight:` (SF) for the UI roles,
/// `kCTFontUIFontUserFixedPitch` for `monospace`, family descriptors for named
/// families, and `CTFontCreateForString(…, "한")` for Hangul.
pub(crate) fn system_fonts(request: &FontRequest) -> SystemFonts {
    autoreleasepool(|_| {
        let language = bcp47(&request.locale);
        let language = if language.is_empty() { crate::apple::preferred_language() } else { language };
        resolve(&mut CoreText { language, sfnts: HashMap::new() }, request)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_core_foundation_results_are_not_released() {
        assert!(Cf::new(std::ptr::null()).is_none());
        assert!(Cf::retain(std::ptr::null_mut()).is_none());
    }
}
