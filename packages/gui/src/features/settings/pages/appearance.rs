//! Original `appearance-section.tsx` (Flutter `AppearanceSettings`): theme,
//! fonts committed on blur/submit with the original clamps, content width,
//! syntax theme and a live preview driven by the in-progress field drafts.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use makepad_widgets::*;
use serde_json::Value;

use super::View;
use super::app::{Pick, select_ctl_lead};
use crate::features::settings::i18n::{t, ta};
use crate::features::settings::json::str_of;
use crate::features::settings::model::preferences::*;
use crate::features::settings::platform::MOBILE;
use crate::features::settings::rpc::{Env, handler};
use crate::features::settings::state::Settings;
use crate::features::settings::ui::draw::{Font, Icon};
use crate::features::settings::ui::field::FieldSpec;
use crate::features::settings::ui::kit::{BSize, Ctl, Lead, Ui, Variant, button, row};
use crate::features::settings::ui::theme::{argb, rgb};

const UI_FONT: &str = "pref.field.uiFontFamily";
const MONO_FONT: &str = "pref.field.monoFontFamily";
const UI_SIZE: &str = "pref.field.uiBaseFontSize";
const CONTENT_SIZE: &str = "pref.field.contentFontSize";
const CODE_SIZE: &str = "pref.field.codeFontSize";
const WIDTH: &str = "pref.field.contentMaxWidth";

fn ui_base(prefs: &Value) -> i64 {
    parse_clamped(prefs.get("uiBaseFontSize").unwrap_or(&Value::Null), MIN_UI_FONT, MAX_UI_FONT).unwrap_or(if MOBILE { 15 } else { 14 })
}
fn content_size(prefs: &Value) -> i64 {
    parse_clamped(prefs.get("contentFontSize").unwrap_or(&Value::Null), MIN_CONTENT_FONT, MAX_CONTENT_FONT).unwrap_or(if MOBILE { 16 } else { 15 })
}
fn code_size(prefs: &Value) -> i64 {
    parse_clamped(prefs.get("codeFontSize").unwrap_or(&Value::Null), MIN_CODE_FONT, MAX_CODE_FONT).unwrap_or(12)
}
fn width_override(prefs: &Value) -> Option<i64> {
    parse_clamped(prefs.get("contentMaxWidth").unwrap_or(&Value::Null), MIN_CONTENT_WIDTH, MAX_CONTENT_WIDTH)
}

/// `commitSize`: parse, clamp, write back the shown text, save when changed.
fn commit_size(field: &'static str, key: &'static str, min: i64, max: i64, current: fn(&Value) -> i64) -> crate::features::settings::rpc::Handler {
    handler(move |s, env| {
        let prefs = s.prefs(&env.preferences());
        let now = current(&prefs);
        let next = parse_clamped_text(&env.fields.text(field), min, max).unwrap_or(now);
        env.fields.set_text(env.cx, field, &next.to_string());
        if next != now {
            s.save_pref(env, key, Some(Value::from(next)));
        }
    })
}

/// `commitFamily`: an invalid family restores the saved text.
fn commit_family(field: &'static str, key: &'static str) -> crate::features::settings::rpc::Handler {
    handler(move |s, env| {
        let prefs = s.prefs(&env.preferences());
        let saved = str_of(&prefs, key).to_string();
        match sanitize_font_family(&env.fields.text(field)) {
            None => env.fields.set_text(env.cx, field, &saved),
            Some(clean) => {
                env.fields.set_text(env.cx, field, &clean);
                if clean != saved {
                    s.save_pref(env, key, Some(Value::String(clean)));
                }
            }
        }
    })
}

/// `commitWidth`: typing the width already in effect keeps following the default.
fn commit_width() -> crate::features::settings::rpc::Handler {
    handler(move |s, env| {
        let prefs = s.prefs(&env.preferences());
        let now = width_override(&prefs).unwrap_or(DEFAULT_CONTENT_WIDTH);
        let next = parse_clamped_text(&env.fields.text(WIDTH), MIN_CONTENT_WIDTH, MAX_CONTENT_WIDTH).unwrap_or(now);
        env.fields.set_text(env.cx, WIDTH, &next.to_string());
        if next != now {
            s.save_pref(env, "contentMaxWidth", Some(Value::from(next)));
        }
    })
}

fn numeric_field(id: &'static str, value: i64, width: f64, commit: crate::features::settings::rpc::Handler, muted: Vec4) -> Ctl {
    let spec = FieldSpec::new(id, value.to_string()).numeric().on_submit(commit.clone()).on_blur(commit);
    Ctl::Row(
        vec![
            Ctl::Field { spec, w: width, h: 36.0 },
            Ctl::Label { text: "px".into(), color: muted, size: 14.0, font: Font::Regular, max_w: 40.0 },
        ],
        8.0,
    )
}

fn family_field(id: &'static str, value: &str, commit: crate::features::settings::rpc::Handler) -> Ctl {
    let spec = FieldSpec::new(id, value).placeholder(t("settings.appearance.fonts.systemDefault")).on_submit(commit.clone()).on_blur(commit);
    Ctl::Field { spec, w: 220.0, h: 36.0 }
}

fn theme_lead(theme: &str, swatch: Option<u32>) -> Lead {
    match theme {
        "light" => Lead::Icon(Icon::Sun, None),
        "dark" => Lead::Icon(Icon::Moon, None),
        "auto" => Lead::Icon(Icon::Monitor, None),
        _ => Lead::Swatch(swatch.map(rgb)),
    }
}

pub fn draw_appearance(ui: &mut Ui, _s: &Settings, v: &View) {
    let prefs = &v.prefs;
    let muted = ui.pal.muted;
    let theme = normalized_theme(prefs.get("theme").unwrap_or(&Value::Null));
    ui.section_title(&t("settings.appearance.theme.title"), None, None);
    ui.card_begin();
    let options: Vec<(String, String)> = THEMES.iter().map(|(id, _)| (id.to_string(), t(&format!("settings.appearance.theme.options.{id}")))).collect();
    let leads: Vec<(String, Lead, bool)> = THEMES.iter().map(|(id, swatch)| (id.to_string(), theme_lead(id, *swatch), *id == "zinc")).collect();
    let pick: Pick = Rc::new(|s: &mut Settings, env: &mut Env, value: String| s.save_pref(env, "theme", Some(Value::String(value))));
    let title = ta("settings.appearance.theme.accessibilityLabel", &[("value", &t(&format!("settings.appearance.theme.options.{theme}")))]);
    let ctl = select_ctl_lead("pref.theme", title, &theme, options, leads, 200.0, true, pick);
    ui.row(row(t("settings.appearance.theme.title")).control(ctl));
    ui.card_end();
    ui.gap(24.0);

    ui.section_title(&t("settings.appearance.fonts.title"), None, None);
    ui.card_begin();
    if !MOBILE {
        let ctl = family_field(UI_FONT, str_of(prefs, "uiFontFamily"), commit_family(UI_FONT, "uiFontFamily"));
        ui.row(row(t("settings.appearance.fonts.interfaceFont")).hint(t("settings.appearance.fonts.interfaceFontHint")).inline().control(ctl));
    }
    let ctl = numeric_field(UI_SIZE, ui_base(prefs), 64.0, commit_size(UI_SIZE, "uiBaseFontSize", MIN_UI_FONT, MAX_UI_FONT, ui_base), muted);
    ui.row(row(t("settings.appearance.fonts.interfaceSize")).hint(t("settings.appearance.fonts.interfaceSizeHint")).control(ctl));
    let ctl = numeric_field(CONTENT_SIZE, content_size(prefs), 64.0, commit_size(CONTENT_SIZE, "contentFontSize", MIN_CONTENT_FONT, MAX_CONTENT_FONT, content_size), muted);
    ui.row(row(t("settings.appearance.fonts.contentSize")).hint(t("settings.appearance.fonts.contentSizeHint")).control(ctl));
    let ctl = family_field(MONO_FONT, str_of(prefs, "monoFontFamily"), commit_family(MONO_FONT, "monoFontFamily"));
    ui.row(row(t("settings.appearance.fonts.codeFont")).hint(t("settings.appearance.fonts.codeFontHint")).inline().control(ctl));
    let ctl = numeric_field(CODE_SIZE, code_size(prefs), 64.0, commit_size(CODE_SIZE, "codeFontSize", MIN_CODE_FONT, MAX_CODE_FONT, code_size), muted);
    ui.row(row(t("settings.appearance.fonts.codeSize")).hint(t("settings.appearance.fonts.codeSizeHint")).control(ctl));
    ui.card_end();
    ui.gap(24.0);

    ui.section_title(&t("settings.appearance.layout.title"), None, None);
    ui.card_begin();
    let mut controls = Vec::new();
    if width_override(prefs).is_some() {
        let reset = handler(|s, env| {
            s.save_pref(env, "contentMaxWidth", None);
            env.fields.set_text(env.cx, WIDTH, &DEFAULT_CONTENT_WIDTH.to_string());
        });
        controls.push(button("pref.contentMaxWidth.reset", t("settings.appearance.layout.reset"), Some(reset)).variant(Variant::Ghost).size(BSize::Sm).ctl());
    }
    controls.push(numeric_field(WIDTH, width_override(prefs).unwrap_or(DEFAULT_CONTENT_WIDTH), 80.0, commit_width(), muted));
    ui.row(row(t("settings.appearance.layout.contentWidth")).hint(t("settings.appearance.layout.contentWidthHint")).control(Ctl::Row(controls, 8.0)));
    ui.card_end();
    ui.gap(24.0);

    let syntax = SYNTAX_THEMES.iter().find(|(id, _)| str_of(prefs, "syntaxTheme") == *id).map(|(id, _)| *id).unwrap_or("one");
    ui.section_title(&t("settings.appearance.syntax.title"), None, None);
    ui.card_begin();
    let options = SYNTAX_THEMES.iter().map(|(id, label)| (id.to_string(), label.to_string())).collect();
    let pick: Pick = Rc::new(|s: &mut Settings, env: &mut Env, value: String| s.save_pref(env, "syntaxTheme", Some(Value::String(value))));
    let label = SYNTAX_THEMES.iter().find(|(id, _)| *id == syntax).map(|(_, l)| *l).unwrap_or("One");
    let title = ta("settings.appearance.syntax.highlightThemeAccessibility", &[("value", label)]);
    let ctl = select_ctl_lead("pref.syntaxTheme", title, syntax, options, Vec::new(), 200.0, true, pick);
    ui.row(row(t("settings.appearance.syntax.highlightTheme")).hint(t("settings.appearance.syntax.highlightThemeHint")).control(ctl));
    ui.card_end();
    ui.gap(12.0 + 16.0);

    // Live preview from the in-progress drafts.
    let content_px = ui.fields.text(CONTENT_SIZE).parse::<f64>().ok().unwrap_or(content_size(prefs) as f64);
    let code_px = ui.fields.text(CODE_SIZE).parse::<f64>().ok().unwrap_or(code_size(prefs) as f64);
    draw_preview(ui, syntax, content_px.clamp(8.0, 32.0), code_px.clamp(8.0, 32.0));
    ui.gap(24.0);
}

const BEFORE: [&str; 5] = [
    "// Format a price for display",
    "export function formatPrice(cents: number) {",
    "  const amount = cents / 100;",
    "  return \"$\" + amount;",
    "}",
];
const AFTER: [&str; 5] = [
    "// Format a price for display",
    "export function formatPrice(cents: number): string {",
    "  const amount = cents / 100;",
    "  return `$${amount.toFixed(2)}`;",
    "}",
];
const CHANGED: [usize; 2] = [1, 3];

fn parse_hex(hex: &str) -> Option<Vec4> {
    let raw = hex.trim_start_matches('#');
    let value = u32::from_str_radix(raw, 16).ok()?;
    Some(match raw.len() {
        6 => rgb(value),
        8 => argb(((value & 0xff) << 24) | (value >> 8)),
        _ => return None,
    })
}

/// One preview line's highlighted `(text, style)` spans.
type PreviewTokens = Vec<(String, Option<paseo_highlight::HighlightStyle>)>;

thread_local! {
    static TOKENS: RefCell<HashMap<String, PreviewTokens>> = RefCell::new(HashMap::new());
}

fn tokens(line: &str) -> PreviewTokens {
    TOKENS.with(|cache| {
        if let Some(found) = cache.borrow().get(line) {
            return found.clone();
        }
        let parsed: PreviewTokens = match paseo_highlight::highlight_line(line, "preview.ts") {
            Ok(tokens) => tokens.into_iter().map(|tok| (tok.text.to_string(), tok.style)).collect(),
            Err(_) => vec![(line.to_string(), None)],
        };
        cache.borrow_mut().insert(line.to_string(), parsed.clone());
        parsed
    })
}

/// Original `appearance-preview.tsx`: content sample plus a unified diff of a
/// fixed snippet in the code font and selected syntax theme.
fn draw_preview(ui: &mut Ui, syntax: &str, content_px: f64, code_px: f64) {
    let pal = ui.pal;
    let scheme = if pal.dark { paseo_highlight::ColorScheme::Dark } else { paseo_highlight::ColorScheme::Light };
    let id = paseo_highlight::SyntaxThemeId::from_name(syntax).unwrap_or(paseo_highlight::SyntaxThemeId::One);
    let colors = paseo_highlight::resolve_syntax_colors(id, scheme);
    let sample = t("settings.appearance.syntax.previewContent");
    let sample_size = ui.measure(Font::Regular, content_px, &sample, Some(ui.w - 24.0));
    let line_h = (code_px * 1.5).round();
    let rows = BEFORE.len() + CHANGED.len();
    let h = 8.0 + sample_size.y + 8.0 + rows as f64 * line_h + 8.0;
    let (x, y, w) = (ui.x, ui.y, ui.w);
    ui.rect(Rect { pos: dvec2(x, y), size: dvec2(w, h) }, pal.surface1, Some(pal.border), 8.0);
    ui.text(Font::Regular, content_px, pal.foreground, dvec2(x + 12.0, y + 8.0), Some(w - 24.0), &sample);
    let mut ly = y + 8.0 + sample_size.y + 8.0;
    let geo = LineGeo { x, w, line_h, code_px, colors, foreground: pal.foreground };
    for i in 0..BEFORE.len() {
        if CHANGED.contains(&i) {
            preview_line(ui, &geo, "- ", BEFORE[i], Some(argb(0x1af85149)), pal.removed, ly);
            ly += line_h;
            preview_line(ui, &geo, "+ ", AFTER[i], Some(argb(0x262ea043)), pal.added, ly);
            ly += line_h;
        } else {
            preview_line(ui, &geo, "  ", BEFORE[i], None, pal.muted, ly);
            ly += line_h;
        }
    }
    ui.y += h;
}

struct LineGeo {
    x: f64,
    w: f64,
    line_h: f64,
    code_px: f64,
    colors: paseo_highlight::SyntaxColors,
    foreground: Vec4,
}

fn preview_line(ui: &mut Ui, g: &LineGeo, marker: &str, text: &str, tint: Option<Vec4>, marker_color: Vec4, ly: f64) {
    if let Some(tint) = tint {
        ui.rect(Rect { pos: dvec2(g.x + 1.0, ly), size: dvec2(g.w - 2.0, g.line_h) }, tint, None, 0.0);
    }
    let ty = ly + (g.line_h - g.code_px * 1.35) / 2.0;
    let mut cx = g.x + 12.0;
    cx += ui.text(Font::Code, g.code_px, marker_color, dvec2(cx, ty), None, marker).x;
    for (piece, style) in tokens(text) {
        let color = style.and_then(|role| parse_hex(g.colors.color(role))).unwrap_or(g.foreground);
        cx += ui.text(Font::Code, g.code_px, color, dvec2(cx, ty), None, &piece).x;
        if cx > g.x + g.w - 12.0 {
            break;
        }
    }
}
