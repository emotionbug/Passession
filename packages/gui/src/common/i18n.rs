//! Port of `lib/ui/localization.dart`: the Flutter catalogs (same keys), `{{name}}`
//! interpolation and count-based plural suffixes, English then key fallback.

use std::cell::Cell;

use serde_json::Value;

const EN: &str = include_str!("../../resources/locales/en.json");
const KO: &str = include_str!("../../resources/locales/ko.json");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lang {
    En,
    Ko,
}

thread_local! {
    static CATALOGS: (Value, Value) = (
        serde_json::from_str(EN).expect("bundled en.json is valid JSON"),
        serde_json::from_str(KO).expect("bundled ko.json is valid JSON"),
    );
    static LANG: Cell<Lang> = Cell::new(system_lang());
}

/// Canonical language setting for a stored `language` value (`paseoLanguagePreference`):
/// `system`, `en` or `ko`; region/script variants keep their language, anything else is
/// English. The stored value is never rewritten.
pub fn language_preference(stored: Option<&Value>) -> &'static str {
    let Some(Value::String(value)) = stored else { return "system" };
    let value = value.trim();
    if value.is_empty() || value == "system" || value == "auto" {
        return "system";
    }
    let language = value.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    if language == "ko" { "ko" } else { "en" }
}

fn system_lang() -> Lang {
    match sys_locale::get_locale() {
        Some(tag) if tag.split(['-', '_']).next().is_some_and(|l| l.eq_ignore_ascii_case("ko")) => Lang::Ko,
        _ => Lang::En,
    }
}

/// Applies `preferences.language`; returns whether the active catalog changed.
pub fn set_language(stored: Option<&Value>) -> bool {
    let preference = language_preference(stored);
    let lang = match preference {
        "ko" => Lang::Ko,
        "en" => Lang::En,
        _ => system_lang(),
    };
    LANG.with(|current| {
        let changed = current.get() != lang;
        current.set(lang);
        changed
    })
}

/// `"en"` or `"ko"`.
pub fn locale_tag() -> &'static str {
    match LANG.with(Cell::get) {
        Lang::En => "en",
        Lang::Ko => "ko",
    }
}

/// A short calendar date without the year in the active language: "Jan 5" / "1월 5일".
/// `month0` is 0-based (chrono `month0`).
pub fn month_day(month0: u32, day: u32) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    match LANG.with(Cell::get) {
        Lang::Ko => format!("{}월 {day}일", month0 + 1),
        Lang::En => format!("{} {day}", MONTHS[month0 as usize % 12]),
    }
}

fn lookup<'a>(catalog: &'a Value, key: &str) -> Option<&'a str> {
    let mut current = catalog;
    for part in key.split('.') {
        current = current.get(part)?;
    }
    current.as_str()
}

/// Plural category, Dart `Intl.pluralLogic` explicit cases first (0/1/2), then CLDR
/// (English: one/other, Korean: other).
fn plural_suffixes(count: f64, lang: Lang) -> Vec<&'static str> {
    let mut suffixes = Vec::with_capacity(2);
    if count == 0.0 {
        suffixes.push("zero");
    } else if count == 1.0 {
        suffixes.push("one");
    } else if count == 2.0 {
        suffixes.push("two");
    }
    let category = match lang {
        Lang::En if count == 1.0 => "one",
        _ => "other",
    };
    if !suffixes.contains(&category) {
        suffixes.push(category);
    }
    suffixes
}

fn template(key: &str, count: Option<f64>) -> String {
    let lang = LANG.with(Cell::get);
    CATALOGS.with(|(en, ko)| {
        let primary = match lang {
            Lang::Ko => ko,
            Lang::En => en,
        };
        let suffixes = count.map(|c| plural_suffixes(c, lang)).unwrap_or_default();
        for catalog in [primary, en] {
            for suffix in &suffixes {
                if let Some(found) = lookup(catalog, &format!("{key}_{suffix}")) {
                    return found.to_string();
                }
            }
            if let Some(found) = lookup(catalog, key) {
                return found.to_string();
            }
        }
        key.to_string()
    })
}

fn interpolate(template: &str, args: &[(&str, &str)]) -> String {
    if args.is_empty() || !template.contains("{{") {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) if !after[..end].contains('{') => {
                let name = after[..end].trim();
                match args.iter().find(|(k, _)| *k == name) {
                    Some((_, value)) => out.push_str(value),
                    None => out.push_str(&rest[start..start + 2 + end + 2]),
                }
                rest = &after[end + 2..];
            }
            _ => {
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Translated string for a dotted catalog key.
pub fn tr(key: &str) -> String {
    template(key, None)
}

/// `{{name}}` interpolation; a numeric `count` argument selects the plural form.
pub fn tr_args(key: &str, args: &[(&str, &str)]) -> String {
    let count = args
        .iter()
        .find(|(k, _)| *k == "count")
        .and_then(|(_, v)| v.trim().parse::<f64>().ok());
    interpolate(&template(key, count), args)
}

pub fn tr_count(key: &str, count: i64, args: &[(&str, &str)]) -> String {
    let count_text = count.to_string();
    let mut all: Vec<(&str, &str)> = Vec::with_capacity(args.len() + 1);
    all.push(("count", count_text.as_str()));
    all.extend(args.iter().filter(|(k, _)| *k != "count").copied());
    interpolate(&template(key, Some(count as f64)), &all)
}
