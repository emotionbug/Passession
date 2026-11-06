//! Palette values ported from `packages/flutter_app/lib/ui/theme.dart`
//! (`_darkPalettes`, `_lightPalette`, `_borderAccents`, PaseoColors status
//! tones). Selected by the `theme` preference, `auto` following the system.

use makepad_widgets::*;

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    pub surface0: Vec4,
    pub surface1: Vec4,
    pub surface2: Vec4,
    pub surface3: Vec4,
    pub sidebar: Vec4,
    pub foreground: Vec4,
    pub muted: Vec4,
    pub extra_muted: Vec4,
    pub border: Vec4,
    pub border_accent: Vec4,
    pub accent: Vec4,
    pub on_accent: Vec4,
    pub success: Vec4,
    pub danger: Vec4,
    pub warning: Vec4,
    pub added: Vec4,
    pub removed: Vec4,
    pub field_error: Vec4,
    pub interaction: Vec4,
    pub sidebar_hover: Vec4,
    pub transparent: Vec4,
}

pub fn rgb(value: u32) -> Vec4 {
    argb(0xff00_0000 | value)
}

pub fn argb(value: u32) -> Vec4 {
    vec4(
        ((value >> 16) & 0xff) as f32 / 255.0,
        ((value >> 8) & 0xff) as f32 / 255.0,
        (value & 0xff) as f32 / 255.0,
        ((value >> 24) & 0xff) as f32 / 255.0,
    )
}

pub fn alpha(color: Vec4, a: f32) -> Vec4 {
    vec4(color.x, color.y, color.z, color.w * a)
}

/// The settings view of the shell palette (`crate::theme::current_palette`,
/// which already resolves `theme` and `auto` against the system appearance).
pub fn palette() -> Palette {
    let p = crate::theme::current_palette();
    Palette {
        dark: p.is_dark,
        surface0: p.surface0,
        surface1: p.surface1,
        surface2: p.surface2,
        surface3: p.surface3,
        sidebar: p.sidebar,
        foreground: p.foreground,
        muted: p.muted,
        extra_muted: p.extra_muted,
        border: p.border,
        border_accent: p.border_accent,
        accent: p.accent,
        on_accent: p.on_tertiary,
        success: p.success,
        danger: p.danger,
        warning: p.warning,
        added: p.added,
        removed: p.removed,
        field_error: p.field_error,
        interaction: p.interaction_highlight,
        sidebar_hover: p.sidebar_hover,
        transparent: vec4(0.0, 0.0, 0.0, 0.0),
    }
}

/// IDENTITY_COLORS fill tones for host swatches.
pub fn identity_fill(name: &str) -> Option<Vec4> {
    Some(rgb(match name {
        "violet" => 0x7a6aa8,
        "sky" => 0x3d7ea6,
        "emerald" => 0x388068,
        "orange" => 0xa4673a,
        "pink" => 0xb05c80,
        "indigo" => 0x6a70b8,
        "teal" => 0x368080,
        "red" => 0xb06260,
        "amber" => 0x8f7838,
        "blue" => 0x5179b0,
        _ => return None,
    }))
}

/// `styles/identity-colors.ts` foregrounds (light, dark) for profile glyphs.
pub fn identity_foreground(name: &str, dark: bool) -> Option<Vec4> {
    let (light, darkc) = match name {
        "violet" => (0x6d49b5, 0xa392d5),
        "sky" => (0x3d6985, 0x6aa6ce),
        "emerald" => (0x3e6e5d, 0x6cae96),
        "orange" => (0x845838, 0xcc8f64),
        "pink" => (0x974168, 0xd87da3),
        "indigo" => (0x5251c2, 0x9299d5),
        "teal" => (0x3e6d6c, 0x6cacab),
        "red" => (0x9c4243, 0xd88381),
        "amber" => (0x716239, 0xb29d64),
        "blue" => (0x39649e, 0x7ba1d5),
        _ => return None,
    };
    Some(rgb(if dark { darkc } else { light }))
}

pub const IDENTITY_COLORS: [&str; 10] = ["violet", "sky", "emerald", "orange", "pink", "indigo", "teal", "red", "amber", "blue"];
