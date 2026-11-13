//! Port of `lib/ui/theme.dart`: the built-in Paseo palettes, typography axes and control
//! geometry, resolved at runtime from `preferences.theme` and the OS appearance.
//!
//! Makepad DSL colours are fixed at load time, so every Paseo primitive copies the current
//! [`Palette`] into its instance fields on each draw. [`epoch`] changes whenever the palette or
//! typography changes; widgets that push values into stock DSL widgets with `apply_over` use
//! [`apply_if_changed`] to do that only once per change.

use std::cell::RefCell;

use makepad_widgets::*;
use serde_json::Value;

use crate::common::UiContext;

live_design! {
    use link::shaders::*;
    use link::theme::*;

    // Latin IBM Plex (bundled by makepad-widgets), Hangul Noto Sans KR (packages/gui/resources),
    // CJK LXGW and colour emoji (bundled by makepad-widgets). Order is the fallback order.
    pub PASEO_FONT_REGULAR = <THEME_FONT_REGULAR> {}
    pub PASEO_FONT_MEDIUM = <THEME_FONT_BOLD> {}
    pub PASEO_FONT_SEMIBOLD = <THEME_FONT_BOLD> {}
    pub PASEO_FONT_BOLD = <THEME_FONT_BOLD> {}
    pub PASEO_FONT_MONO = <THEME_FONT_CODE> {}
    // Lucide 300 weight, the glyph set of the original lucide_icons_flutter `*300` icons.
    pub PASEO_FONT_ICON = {
        font_family: {
            lucide = font("crate://self/resources/fonts/Lucide-w300.ttf", 0.0, 0.0)
        },
        line_spacing: 1.0
    }

    // First-paint defaults (dark palette); runtime colours come from `Palette`.
    pub PASEO_DARK_SURFACE0 = #x181b1a
    pub PASEO_DARK_SURFACE1 = #x1e2120
    pub PASEO_DARK_SURFACE2 = #x272a29
    pub PASEO_DARK_SIDEBAR = #x141716
    pub PASEO_DARK_FOREGROUND = #fafafa
    pub PASEO_DARK_MUTED = #a1a5a4

    // Rounded rectangle with optional inside border; all colours are per-instance so one draw
    // call serves every surface regardless of palette. Uses an exact signed distance:
    // `Sdf2d::box` doubles the radius and is not a true distance inside the shape, so square
    // boxes come out nearly transparent and pill radii turn into chamfered ends.
    pub DrawBox = {{DrawBox}} {
        fn pixel(self) -> vec4 {
            let size = self.rect_size;
            let p = self.pos * size - size * 0.5;
            let r = min(self.radius, min(size.x, size.y) * 0.5);
            let q = abs(p) - size * 0.5 + vec2(r, r);
            let d = length(max(q, vec2(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
            let outer = clamp(0.5 - d, 0.0, 1.0);
            let fill = vec4(self.color.rgb * self.color.a, self.color.a);
            let bw = self.border_width;
            if bw > 0.0 {
                let border = vec4(self.border_color.rgb * self.border_color.a, self.border_color.a);
                let inner = clamp(0.5 - (d + bw), 0.0, 1.0);
                return mix(border, fill, inner) * outer * self.opacity;
            }
            return fill * outer * self.opacity;
        }
    }

    // Ring/arc used by status marks and spinners: `progress` 0..1 of the ring is drawn
    // starting at `rotation` (turns).
    pub DrawRing = {{DrawRing}} {
        fn pixel(self) -> vec4 {
            let sdf = Sdf2d::viewport(self.pos * self.rect_size);
            let c = self.rect_size * 0.5;
            let r = min(c.x, c.y) - self.stroke * 0.5 - 0.5;
            if self.progress >= 1.0 {
                sdf.circle(c.x, c.y, r);
                sdf.stroke(self.color, self.stroke);
                return sdf.result;
            }
            let start = self.rotation * 6.283185;
            let end = start + self.progress * 6.283185;
            sdf.arc_round_caps(c.x, c.y, r, start, end, self.stroke);
            sdf.fill(self.color);
            return sdf.result;
        }
    }
}

#[derive(Live, LiveHook, LiveRegister)]
#[repr(C)]
pub struct DrawBox {
    #[deref]
    pub draw_super: DrawQuad,
    #[live]
    pub color: Vec4,
    #[live]
    pub border_color: Vec4,
    #[live(0.0)]
    pub border_width: f32,
    #[live(0.0)]
    pub radius: f32,
    #[live(1.0)]
    pub opacity: f32,
}

impl DrawBox {
    /// Draws one box in absolute coordinates at the current `opacity` (1 unless a painter
    /// fades, `Painter::set_alpha`).
    pub fn paint(&mut self, cx: &mut Cx2d, rect: Rect, fill: Vec4, radius: f64) {
        self.color = fill;
        self.border_width = 0.0;
        self.radius = radius as f32;
        self.draw_abs(cx, rect);
    }

    pub fn paint_bordered(
        &mut self,
        cx: &mut Cx2d,
        rect: Rect,
        fill: Vec4,
        border: Vec4,
        border_width: f64,
        radius: f64,
    ) {
        self.color = fill;
        self.border_color = border;
        self.border_width = border_width as f32;
        self.radius = radius as f32;
        self.draw_abs(cx, rect);
    }
}

#[derive(Live, LiveHook, LiveRegister)]
#[repr(C)]
pub struct DrawRing {
    #[deref]
    pub draw_super: DrawQuad,
    #[live]
    pub color: Vec4,
    #[live(1.5)]
    pub stroke: f32,
    #[live(1.0)]
    pub progress: f32,
    #[live(0.0)]
    pub rotation: f32,
}

/// Flutter `PaseoColors` plus the `ColorScheme` entries widgets read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub is_dark: bool,
    pub surface0: Vec4,
    pub surface1: Vec4,
    pub surface2: Vec4,
    pub surface3: Vec4,
    pub surface4: Vec4,
    pub sidebar: Vec4,
    /// Original `paseoSidebarHover`: the row hover fill in the sidebar.
    pub sidebar_hover: Vec4,
    pub selected: Vec4,
    pub muted: Vec4,
    pub extra_muted: Vec4,
    pub accent: Vec4,
    pub accent_bright: Vec4,
    pub added: Vec4,
    pub removed: Vec4,
    pub success: Vec4,
    pub danger: Vec4,
    pub warning: Vec4,
    pub running: Vec4,
    pub border: Vec4,
    pub border_accent: Vec4,
    pub foreground: Vec4,
    pub primary: Vec4,
    pub on_primary: Vec4,
    /// `ColorScheme.onTertiary`: text on the accent (original `accentForeground`).
    pub on_tertiary: Vec4,
    /// `ColorScheme.error`.
    pub destructive: Vec4,
    /// Original `interactionHighlight`: icon-button hover/press fill.
    pub interaction_highlight: Vec4,
    /// Original palette red-300 field error line.
    pub field_error: Vec4,
    /// Modal scrim behind dialogs and sheets.
    pub scrim: Vec4,
    /// Merged pull request purple.
    pub merged: Vec4,
    /// Window/scaffold background (`scaffoldBackgroundColor`).
    pub background: Vec4,
    /// Dialog surface (`dialogTheme.backgroundColor`).
    pub dialog: Vec4,
}

/// Flutter `PaseoTypography`: independent UI/content/code size axes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Typography {
    pub ui_base_font_size: f64,
    pub content_font_size: f64,
    pub code_font_size: f64,
    pub content_max_width: f64,
    pub mobile: bool,
    authored_ui_base_font_size: f64,
    system_text_scale: paseo_gui_platform::appearance::TextScale,
}

impl Typography {
    /// Scale a text-only metric (for example an authored fixed line height),
    /// without changing spacing, display density, or app font preferences.
    pub fn system_size(&self, authored_px: f64) -> f64 {
        self.system_text_scale.logical_size(authored_px)
    }

    /// `uiSize`: an authored size at the 14px base, scaled by `uiBaseFontSize`.
    pub fn ui_size(&self, authored_px: f64) -> f64 {
        self.system_text_scale.logical_size((authored_px * self.authored_ui_base_font_size / 14.0).round())
    }

    /// Logical pixels to a Makepad `font_size` (points at 96 dpi).
    pub fn pt(px: f64) -> f32 {
        (px * 0.75) as f32
    }

    /// Authored UI size straight to Makepad points.
    pub fn ui_pt(&self, authored_px: f64) -> f32 {
        Self::pt(self.ui_size(authored_px))
    }
}

/// Original `control-geometry.ts` sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Live, LiveHook)]
#[live_ignore]
pub enum ControlSize {
    Xs,
    Sm,
    #[pick]
    Md,
    Lg,
}

pub fn control_height(size: ControlSize) -> f64 {
    let (height, font) = match size {
        ControlSize::Xs => (28.0, 12.0),
        ControlSize::Sm => (32.0, 14.0),
        ControlSize::Md | ControlSize::Lg => (44.0, 14.0),
    };
    let typography = current_typography();
    // Grow only for the extra text height, not the display-density scale.
    let authored = (font * typography.authored_ui_base_font_size / 14.0).round();
    height + (typography.ui_size(font) - authored).max(0.0) * 1.5
}

pub fn control_icon_size(size: ControlSize) -> f64 {
    match size {
        ControlSize::Xs => 12.0,
        ControlSize::Sm => 14.0,
        ControlSize::Md => 16.0,
        ControlSize::Lg => 20.0,
    }
}

/// Flutter colour literal `0xAARRGGBB`.
pub const fn argb(value: u32) -> Vec4 {
    Vec4 {
        x: ((value >> 16) & 0xff) as f32 / 255.0,
        y: ((value >> 8) & 0xff) as f32 / 255.0,
        z: (value & 0xff) as f32 / 255.0,
        w: ((value >> 24) & 0xff) as f32 / 255.0,
    }
}

/// `color.withValues(alpha: a)`.
pub fn with_alpha(color: Vec4, alpha: f32) -> Vec4 {
    Vec4 { w: alpha, ..color }
}

/// Multiplies the existing alpha (Flutter `Opacity`).
pub fn fade(color: Vec4, opacity: f32) -> Vec4 {
    Vec4 { w: color.w * opacity, ..color }
}

/// The selection style of a Makepad `TextInput`, for `apply_over`. Its shader reads a
/// different colour per state (focused, hovered, pressed, empty, disabled), and Makepad's
/// defaults for those are faint white tints for its own dark theme: a field that set only
/// `color` showed no selection while focused on a light surface. Every state gets `color`.
pub fn text_selection(color: Vec4) -> Vec<LiveNode> {
    live! {
        draw_selection: {
            color: (color), color_hover: (color), color_focus: (color),
            color_down: (color), color_empty: (color), color_disabled: (color)
        }
    }
    .to_vec()
}

struct Raw {
    surfaces: [u32; 5],
    sidebar: u32,
    muted: u32,
    extra_muted: u32,
    border: u32,
    accent: u32,
    accent_bright: u32,
    destructive: u32,
    status: Status,
}

/// Status text colours: diff removal, danger, warning and success. They are text on every
/// surface, so a palette with lighter surfaces (ghostty) lifts them to keep 4.5:1.
struct Status {
    removed: u32,
    danger: u32,
    warning: u32,
    success: u32,
}

const DARK_STATUS: Status = Status { removed: 0xfff87171, danger: 0xffd8847b, warning: 0xffc09664, success: 0xff6cb17b };
const LIGHT_STATUS: Status = Status { removed: 0xffb91c1c, danger: 0xff9d433b, warning: 0xff7b5d39, success: 0xff3e704a };

fn dark_raw(name: &str) -> (Raw, u32) {
    match name {
        "zinc" => (
            Raw {
                surfaces: [0xff18181b, 0xff1f1f22, 0xff27272a, 0xff3f3f46, 0xff52525b],
                sidebar: 0xff131316,
                muted: 0xffa1a1aa,
                extra_muted: 0xff71717a,
                border: 0xff27272a,
                accent: 0xffe4e4e7,
                accent_bright: 0xfffafafa,
                destructive: 0xffc44a4a,
                status: DARK_STATUS,
            },
            0xff303036,
        ),
        "midnight" => (
            Raw {
                surfaces: [0xff161820, 0xff1c1e27, 0xff252731, 0xff3c3e4c, 0xff535564],
                sidebar: 0xff121420,
                muted: 0xff9a9db0,
                extra_muted: 0xff71748a,
                border: 0xff242636,
                accent: 0xff3b6fcf,
                accent_bright: 0xff7eaaeb,
                destructive: 0xffc44a52,
                status: DARK_STATUS,
            },
            0xff2e3040,
        ),
        "claude" => (
            Raw {
                surfaces: [0xff1f1f1e, 0xff262523, 0xff2f2d2b, 0xff4a4745, 0xff605d5b],
                sidebar: 0xff1a1918,
                muted: 0xffada9a5,
                extra_muted: 0xff7e7a75,
                border: 0xff2c2a27,
                accent: 0xffd97757,
                accent_bright: 0xffe89a7f,
                destructive: 0xffbd4636,
                status: DARK_STATUS,
            },
            0xff36332f,
        ),
        "ghostty" => (
            Raw {
                surfaces: [0xff282c34, 0xff2f333d, 0xff383c48, 0xff4a4f5e, 0xff5b6175],
                sidebar: 0xff21252d,
                muted: 0xffc8ccd8,
                extra_muted: 0xffa0a4b2,
                border: 0xff353a47,
                accent: 0xff89b4fa,
                accent_bright: 0xffb4d0fc,
                destructive: 0xffc44a55,
                status: Status { removed: 0xfffa9090, danger: 0xffe2968e, warning: 0xffcca474, success: 0xff7cc08a },
            },
            0xff3f4454,
        ),
        "pureBlack" => (
            Raw {
                surfaces: [0xff000000, 0xff0a0a0a, 0xff111111, 0xff202020, 0xff2d2d2d],
                sidebar: 0xff000000,
                muted: 0xffa1a1aa,
                extra_muted: 0xff71717a,
                border: 0xff1c1c1c,
                accent: 0xff20744a,
                accent_bright: 0xff7ccba0,
                destructive: 0xffc44a4a,
                status: DARK_STATUS,
            },
            0xff242424,
        ),
        _ => (
            Raw {
                surfaces: [0xff181b1a, 0xff1e2120, 0xff272a29, 0xff434645, 0xff595b5b],
                sidebar: 0xff141716,
                muted: 0xffa1a5a4,
                extra_muted: 0xff717574,
                border: 0xff252b2a,
                accent: 0xff20744a,
                accent_bright: 0xff7ccba0,
                destructive: 0xffc64f43,
                status: DARK_STATUS,
            },
            0xff2f3534,
        ),
    }
}

const LIGHT: Raw = Raw {
    surfaces: [0xffffffff, 0xfffafafa, 0xfff4f4f5, 0xffe4e4e7, 0xffd4d4d8],
    sidebar: 0xfff4f4f5,
    muted: 0xff6b6b74,
    extra_muted: 0xff8a8a92,
    border: 0xffe4e4e7,
    accent: 0xff20744a,
    accent_bright: 0xff1b7a45,
    destructive: 0xffb04138,
    status: LIGHT_STATUS,
};

/// Original THEME_OPTIONS literals: stored names to the built-in palette key.
fn palette_name(theme: &str) -> &str {
    match theme {
        "dark-zinc" => "zinc",
        "dark-midnight" => "midnight",
        "dark-claude" => "claude",
        "dark-ghostty" => "ghostty",
        "dark-pure-black" => "pureBlack",
        other => other,
    }
}

/// Flutter `AppPreferences.themeMode`: `Some(true)` dark, `Some(false)` light, `None` system.
pub fn theme_mode(preferences: &Value) -> Option<bool> {
    let value = preferences
        .get("theme")
        .or_else(|| preferences.get("themeMode"))
        .and_then(Value::as_str)?;
    if value == "dark"
        || value.starts_with("dark-")
        || matches!(value, "zinc" | "midnight" | "claude" | "ghostty" | "pureBlack")
    {
        return Some(true);
    }
    if value == "light" || value.starts_with("light-") {
        return Some(false);
    }
    None
}

/// WCAG 2.x relative luminance of an opaque colour.
pub fn luminance(color: Vec4) -> f32 {
    fn channel(c: f32) -> f32 {
        if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    }
    0.2126 * channel(color.x) + 0.7152 * channel(color.y) + 0.0722 * channel(color.z)
}

/// WCAG 2.x contrast ratio between two opaque colours (1.0..=21.0).
pub fn contrast(a: Vec4, b: Vec4) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `paseoTheme(brightness:, preferences:)` colours.
pub fn palette_for(preferences: &Value, dark: bool) -> Palette {
    let name = palette_name(preferences.get("theme").and_then(Value::as_str).unwrap_or(""));
    let (raw, border_accent) = if dark { dark_raw(name) } else { (LIGHT, 0xffececf1) };
    let foreground = if dark { argb(0xfffafafa) } else { argb(0xff1a1a1e) };
    let accent = argb(raw.accent);
    let surface0 = argb(raw.surfaces[0]);
    let surface1 = argb(raw.surfaces[1]);
    let surface2 = argb(raw.surfaces[2]);
    let surface3 = argb(raw.surfaces[3]);
    let surface4 = argb(raw.surfaces[4]);
    Palette {
        is_dark: dark,
        surface0,
        surface1,
        surface2,
        surface3,
        surface4,
        sidebar: argb(raw.sidebar),
        sidebar_hover: if dark { argb(0x0fffffff) } else { argb(0x0a000000) },
        selected: if dark { surface2 } else { surface4 },
        muted: argb(raw.muted),
        extra_muted: argb(raw.extra_muted),
        accent,
        accent_bright: argb(raw.accent_bright),
        added: if dark { argb(0xff4ade80) } else { argb(0xff15803d) },
        removed: argb(raw.status.removed),
        success: argb(raw.status.success),
        danger: argb(raw.status.danger),
        warning: argb(raw.status.warning),
        running: if dark { argb(0xff5caaf6) } else { argb(0xff268ae0) },
        border: argb(raw.border),
        border_accent: argb(border_accent),
        foreground,
        primary: if dark { foreground } else { argb(0xff18181b) },
        on_primary: if dark { surface0 } else { argb(0xfffafafa) },
        // Text on accent fills: white, unless the accent is light (zinc, ghostty, claude).
        on_tertiary: if contrast(argb(0xffffffff), accent) >= contrast(surface0, accent) { argb(0xffffffff) } else { surface0 },
        destructive: argb(raw.destructive),
        interaction_highlight: if dark { argb(0x14ffffff) } else { argb(0x0f000000) },
        field_error: if dark { argb(0xfffca5a5) } else { argb(0xffb91c1c) },
        scrim: argb(0x73000000),
        merged: if dark { argb(0xffa78bfa) } else { argb(0xff7c3aed) },
        background: if dark { surface1 } else { surface0 },
        dialog: if dark { surface2 } else { surface0 },
    }
}

fn clamp_pref(preferences: &Value, key: &str, fallback: f64, min: f64, max: f64) -> f64 {
    match preferences.get(key).and_then(Value::as_f64) {
        Some(v) if v.is_finite() => v.clamp(min, max),
        _ => fallback,
    }
}

/// `paseoTheme` typography (mobile defaults 15/16).
pub fn typography_for(preferences: &Value, mobile: bool) -> Typography {
    typography_with_text_scale(preferences, mobile, paseo_gui_platform::appearance::TextScale::default())
}

fn typography_with_text_scale(
    preferences: &Value,
    mobile: bool,
    system_text_scale: paseo_gui_platform::appearance::TextScale,
) -> Typography {
    let ui_base = clamp_pref(preferences, "uiBaseFontSize", if mobile { 15.0 } else { 14.0 }, 10.0, 21.0);
    Typography {
        ui_base_font_size: system_text_scale.logical_size(ui_base),
        content_font_size: system_text_scale.logical_size(clamp_pref(preferences, "contentFontSize", if mobile { 16.0 } else { 15.0 }, 10.0, 21.0)),
        code_font_size: system_text_scale.logical_size(clamp_pref(preferences, "codeFontSize", 12.0, 9.0, 22.0)),
        content_max_width: clamp_pref(preferences, "contentMaxWidth", 820.0, 600.0, 4000.0),
        mobile,
        authored_ui_base_font_size: ui_base,
        system_text_scale,
    }
}

/// Android and iOS builds use the compact control geometry everywhere.
pub const MOBILE: bool = cfg!(any(target_os = "android", target_os = "ios"));

struct Current {
    palette: Palette,
    typography: Typography,
    epoch: u64,
}

thread_local! {
    static CURRENT: RefCell<Current> = RefCell::new(Current {
        palette: palette_for(&Value::Null, true),
        typography: typography_for(&Value::Null, MOBILE),
        epoch: 1,
    });
}

/// Shell entry point: recomputes the palette and typography; returns whether anything changed.
pub fn update(preferences: &Value, system_dark: bool) -> bool {
    update_with_text_scale(preferences, system_dark, paseo_gui_platform::appearance::system_text_scale())
}

fn update_with_text_scale(
    preferences: &Value,
    system_dark: bool,
    text_scale: paseo_gui_platform::appearance::TextScale,
) -> bool {
    let dark = theme_mode(preferences).unwrap_or(system_dark);
    let palette = palette_for(preferences, dark);
    let typography = typography_with_text_scale(preferences, MOBILE, text_scale);
    // Stock Makepad scrollbar DSL colors are dark-only. Keep every desktop
    // scrollbar (including nested PortalLists) on the same runtime palette.
    makepad_widgets::scroll_bar::set_desktop_scroll_bar_color(palette.foreground);
    CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        if current.palette == palette && current.typography == typography {
            return false;
        }
        current.palette = palette;
        current.typography = typography;
        current.epoch += 1;
        true
    })
}

pub fn current_palette() -> Palette {
    CURRENT.with(|current| current.borrow().palette)
}

pub fn current_typography() -> Typography {
    CURRENT.with(|current| current.borrow().typography)
}

/// The palette of the UI being dispatched (one palette per process).
pub fn palette(_ui: &UiContext) -> Palette {
    current_palette()
}

pub fn typography(_ui: &UiContext) -> Typography {
    current_typography()
}

pub fn epoch() -> u64 {
    CURRENT.with(|current| current.borrow().epoch)
}

/// Invalidates epoch-keyed caches (layout keys, measured fields) without a palette/typography
/// change, e.g. after fonts are redefined.
pub fn bump_epoch() {
    CURRENT.with(|current| current.borrow_mut().epoch += 1);
}

/// Runs `apply` (typically `widget.apply_over(cx, live!{…})`) once per theme change.
pub fn apply_if_changed(
    seen_epoch: &mut u64,
    apply: impl FnOnce(&Palette, &Typography),
) {
    let now = epoch();
    if *seen_epoch != now {
        *seen_epoch = now;
        apply(&current_palette(), &current_typography());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paseo_gui_platform::appearance::TextScale;
    use serde_json::json;

    fn enlarged_text() -> TextScale {
        TextScale::from_dp([0.0, 16.0, 20.0, 24.0, 28.0, 34.0, 36.0, 40.0, 46.0, 116.0])
    }

    #[test]
    fn system_text_scale_preserves_app_preferences_and_layout_width() {
        let preferences = json!({"uiBaseFontSize": 18, "contentFontSize": 20, "codeFontSize": 10, "contentMaxWidth": 900});
        let normal = typography_with_text_scale(&preferences, true, TextScale::default());
        let enlarged = typography_with_text_scale(&preferences, true, enlarged_text());
        assert_eq!(enlarged.ui_size(14.0), enlarged_text().logical_size(normal.ui_size(14.0)));
        assert_eq!(enlarged.ui_size(12.0), enlarged_text().logical_size(normal.ui_size(12.0)));
        assert_eq!(enlarged.content_font_size, 36.0);
        assert_eq!(enlarged.code_font_size, 20.0);
        assert_eq!(enlarged.content_max_width, normal.content_max_width);
        assert_eq!(Typography::pt(enlarged.content_font_size), 27.0);
        let clamped = typography_with_text_scale(&json!({"contentFontSize": 200}), true, enlarged_text());
        assert_eq!(clamped.content_font_size, enlarged_text().logical_size(21.0));
    }

    #[test]
    fn system_text_scale_invalidates_once_and_restores_metrics() {
        let preferences = json!({"theme": "light", "uiBaseFontSize": 17});
        update_with_text_scale(&preferences, false, TextScale::default());
        let normal = current_typography();
        let original_epoch = epoch();
        let original_height = control_height(ControlSize::Sm);
        assert!(update_with_text_scale(&preferences, false, enlarged_text()));
        assert_eq!(epoch(), original_epoch + 1);
        assert!(control_height(ControlSize::Sm) > original_height);
        assert!(!update_with_text_scale(&preferences, false, enlarged_text()));
        assert_eq!(epoch(), original_epoch + 1);
        assert!(update_with_text_scale(&preferences, false, TextScale::default()));
        assert_eq!(current_typography(), normal);
        assert_eq!(control_height(ControlSize::Sm), original_height);
    }

    /// WCAG minimums for the text and icon tokens on every surface they are drawn on: 4.5:1
    /// for text, 3:1 for icons and focus borders.
    #[test]
    fn every_palette_meets_text_and_icon_contrast() {
        let themes = ["dark", "dark-zinc", "dark-midnight", "dark-claude", "dark-ghostty", "dark-pure-black", "light"];
        let mut failures = Vec::new();
        for theme in themes {
            let p = palette_for(&json!({ "theme": theme }), theme != "light");
            let backgrounds = [("surface0", p.surface0), ("surface1", p.surface1), ("surface2", p.surface2), ("sidebar", p.sidebar), ("dialog", p.dialog)];
            let text = [
                ("foreground", p.foreground),
                ("muted", p.muted),
                ("accent_bright", p.accent_bright),
                ("danger", p.danger),
                ("warning", p.warning),
                ("success", p.success),
                ("field_error", p.field_error),
                ("removed", p.removed),
                ("added", p.added),
            ];
            let icons = [("extra_muted", p.extra_muted), ("running", p.running), ("merged", p.merged)];
            for (bg_name, bg) in backgrounds {
                for (name, color) in text {
                    let ratio = contrast(color, bg);
                    if ratio < 4.5 {
                        failures.push(format!("{theme}: {name} on {bg_name} = {ratio:.2}"));
                    }
                }
                for (name, color) in icons {
                    let ratio = contrast(color, bg);
                    if ratio < 3.0 {
                        failures.push(format!("{theme}: {name} on {bg_name} = {ratio:.2}"));
                    }
                }
            }
            for (name, fg, bg, min) in [
                ("on_tertiary on accent", p.on_tertiary, p.accent, 4.5),
                ("white on destructive", argb(0xffffffff), p.destructive, 4.5),
                ("on_primary on primary", p.on_primary, p.primary, 4.5),
                ("focus border (muted) on surface2", p.muted, p.surface2, 3.0),
            ] {
                let ratio = contrast(fg, bg);
                if ratio < min {
                    failures.push(format!("{theme}: {name} = {ratio:.2}"));
                }
            }
        }
        assert!(failures.is_empty(), "contrast below WCAG minimums:\n{}", failures.join("\n"));
    }
}
