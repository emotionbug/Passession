//! Original `useTimeAgo` / reset-duration wording ("2 hours ago", "in 3 days").

use chrono::{DateTime, Utc};

use crate::features::settings::i18n::{t, ta};

pub fn parse_iso(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.trim()).ok().map(|d| d.with_timezone(&Utc))
}

/// `settingsRelativeTime`: `None` when unparsable; `Some("")` for a future
/// duration that already passed.
pub fn relative(iso: Option<&str>, ago: bool) -> Option<String> {
    let at = parse_iso(iso?)?;
    let now = Utc::now();
    let diff = if ago { now - at } else { at - now };
    let unit = |key: &str, value: i64| {
        let base = if ago { "providerUsage.timing" } else { "providerUsage.duration" };
        ta(&format!("{base}.{key}"), &[("value", &value.to_string())])
    };
    if ago && diff.num_seconds() < 60 {
        return Some(t("providerUsage.timing.justNow"));
    }
    if !ago && diff <= chrono::Duration::zero() {
        return Some(String::new());
    }
    if diff.num_days() > 0 {
        return Some(unit(if ago { "daysAgo" } else { "days" }, diff.num_days()));
    }
    if diff.num_hours() > 0 {
        return Some(unit(if ago { "hoursAgo" } else { "hours" }, diff.num_hours()));
    }
    Some(unit(if ago { "minutesAgo" } else { "minutes" }, diff.num_minutes()))
}

pub fn is_after(a: &str, b: &str) -> bool {
    match (parse_iso(a), parse_iso(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

pub fn is_future(iso: &str) -> bool {
    parse_iso(iso).map(|at| at > Utc::now()).unwrap_or(false)
}
