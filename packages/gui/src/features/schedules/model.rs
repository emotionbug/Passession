//! Original `utils/schedule-format.ts`, `schedule-derivation.ts` and
//! `schedule-cadence-options.ts` as ported in Flutter `schedules_screen.dart`.

use chrono::{DateTime, Datelike, Local, Utc};
use serde_json::{json, Value};

use crate::features::workspace::host::Host;
use crate::features::workspace::i18n::{l, lf, t, ta};
use crate::features::workspace::rpc::{object_of, str_of};

const MS_PER_MINUTE: i64 = 60_000;
const MS_PER_HOUR: i64 = 3_600_000;
const MS_PER_DAY: i64 = 86_400_000;

/// `Heartbeat` for agent targets, else `Schedule`.
pub fn product_name(schedule: &Value) -> &'static str {
    if str_of(object_of(schedule, "target"), "type") == "agent" { l("Heartbeat") } else { l("Schedule") }
}

pub fn product_name_lower(schedule: &Value) -> &'static str {
    if str_of(object_of(schedule, "target"), "type") == "agent" { l("heartbeat") } else { l("schedule") }
}

/// `resolveScheduleTitle`.
pub fn schedule_title(schedule: &Value) -> String {
    let name = str_of(schedule, "name").trim();
    if !name.is_empty() {
        return name.to_owned();
    }
    let target = object_of(schedule, "target");
    if str_of(target, "type") == "new-agent" {
        let title = str_of(object_of(target, "config"), "title").trim();
        if !title.is_empty() {
            return title.to_owned();
        }
    }
    str_of(schedule, "prompt")
        .split('\n')
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| lf("Untitled {product}", &[("product", product_name_lower(schedule))]))
}

/// `shortenPath`: the home directory as `~`.
pub fn shorten_path(path: &str) -> String {
    for prefix in ["/Users/", "/home/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let end = rest.find('/').unwrap_or(rest.len());
            if end > 0 {
                return format!("~{}", &rest[end..]);
            }
        }
    }
    path.to_owned()
}

/// `everyMsToParts`: (value, unit noun).
pub fn every_ms_to_parts(ms: Option<f64>) -> (i64, &'static str) {
    let Some(ms) = ms.filter(|m| m.is_finite() && *m > 0.0) else { return (1, "hour") };
    let whole = ms.round() as i64;
    if ms == whole as f64 && whole % MS_PER_DAY == 0 {
        return (whole / MS_PER_DAY, "day");
    }
    if ms == whole as f64 && whole % MS_PER_HOUR == 0 {
        return (whole / MS_PER_HOUR, "hour");
    }
    let minutes = (ms / MS_PER_MINUTE as f64).round() as i64;
    (minutes.max(1), "minute")
}

/// `formatCadence`.
pub fn format_cadence(cadence: &Value) -> String {
    if str_of(cadence, "type") == "every" {
        let (value, noun) = every_ms_to_parts(cadence.get("everyMs").and_then(Value::as_f64));
        let unit = l(noun);
        return if value == 1 {
            lf("Every 1 {unit}", &[("unit", unit)])
        } else {
            lf("Every {n} {unit}s", &[("n", &value.to_string()), ("unit", unit)])
        };
    }
    let expression = str_of(cadence, "expression");
    let zone = crate::features::workspace::rpc::str_or(cadence, "timezone", "UTC");
    describe_cron(expression, Some(zone)).unwrap_or_else(|| expression.to_owned())
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// `describeCron`: humanizes the fixed-time family, `None` otherwise. `None` zone omits the
/// zone suffix.
pub fn describe_cron(expression: &str, timezone: Option<&str>) -> Option<String> {
    let trimmed = expression.trim();
    if validate_cron(trimmed).is_some() {
        return None;
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let [minute, hour, day_of_month, month, day_of_week] = parts[..] else { return None };
    if [minute, hour, month, day_of_month, day_of_week].iter().all(|p| *p == "*") {
        return Some(l("Every minute").to_owned());
    }
    if !digits(minute) || month != "*" || day_of_month != "*" {
        return None;
    }
    let minute_num: u32 = minute.parse().ok()?;
    if hour == "*" {
        if day_of_week != "*" {
            return None;
        }
        return Some(if minute_num == 0 {
            l("Every hour").to_owned()
        } else {
            lf("Every hour at :{minute}", &[("minute", &format!("{minute_num:02}"))])
        });
    }
    if !digits(hour) {
        return None;
    }
    const DAYS: [&str; 7] = ["Sundays", "Mondays", "Tuesdays", "Wednesdays", "Thursdays", "Fridays", "Saturdays"];
    let day_label = match day_of_week {
        "*" => l("Daily"),
        "1-5" => l("Weekdays"),
        "0,6" | "6,0" => l("Weekends"),
        day if day.len() == 1 && digits(day) && day.parse::<usize>().ok()? < 7 => l(DAYS[day.parse::<usize>().ok()?]),
        _ => return None,
    };
    let time = format!("{:02}:{minute_num:02}", hour.parse::<u32>().ok()?);
    let text = lf("{day} at {time}", &[("day", day_label), ("time", &time)]);
    Some(match timezone {
        Some(zone) => format!("{text} {zone}"),
        None => text,
    })
}

/// `validateCron`: the protocol's `validateCronExpression` messages.
pub fn validate_cron(expression: &str) -> Option<String> {
    let trimmed = expression.trim();
    if trimmed.is_empty() {
        return Some(l("Enter a cron expression").to_owned());
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() != 5 {
        return Some(l("Cron expressions must have 5 fields").to_owned());
    }
    const BOUNDS: [(u32, u32, &str); 5] = [
        (0, 59, "workspace.schedules.cronFields.minute"),
        (0, 23, "workspace.schedules.cronFields.hour"),
        (1, 31, "workspace.schedules.cronFields.dayOfMonth"),
        (1, 12, "workspace.schedules.cronFields.month"),
        (0, 6, "workspace.schedules.cronFields.dayOfWeek"),
    ];
    for (part, (min, max, field)) in parts.iter().zip(BOUNDS) {
        if let Some(error) = cron_field_error(part, min, max, &t(field)) {
            return Some(error);
        }
    }
    None
}

fn cron_field_error(source: &str, min: u32, max: u32, name: &str) -> Option<String> {
    let error = |kind: &'static str| Some(lf(kind, &[("name", name)]));
    for raw in source.split(',') {
        let part = raw.trim();
        if part.is_empty() {
            return error("Invalid {name} field");
        }
        let steps: Vec<&str> = part.split('/').collect();
        if steps.len() > 2 {
            return error("Invalid {name} step");
        }
        if steps.len() == 2 {
            let step = steps[1].trim();
            match step.parse::<u32>() {
                Ok(value) if value > 0 && value.to_string() == step => {}
                _ => return error("Invalid {name} step"),
            }
        }
        let base = steps[0];
        if base == "*" {
            continue;
        }
        if let Some((start, end)) = base.split_once('-') {
            if !digits(start) || !digits(end) {
                return error("Invalid {name} range");
            }
            match (start.parse::<u32>(), end.parse::<u32>()) {
                (Ok(start), Ok(end)) if start <= end && start >= min && end <= max => continue,
                _ => return error("Invalid {name} range"),
            }
        }
        match base.parse::<u32>() {
            Ok(value) if digits(base) && value >= min && value <= max => {}
            _ => return error("Invalid {name} value"),
        }
    }
    None
}

fn parse_time(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let text = value?.as_str()?;
    DateTime::parse_from_rfc3339(text).ok().map(|t| t.with_timezone(&Utc))
}

/// `formatNextRun`: "soon", "in 5m", "in 3h", "in 2d".
pub fn format_next_run(iso: Option<&Value>, now: DateTime<Utc>) -> String {
    let Some(target) = parse_time(iso) else { return String::new() };
    let diff = (target - now).num_milliseconds();
    let round = |value: i64, unit: i64| ((value as f64) / unit as f64).round() as i64;
    if diff < MS_PER_MINUTE {
        l("soon").to_owned()
    } else if diff < MS_PER_HOUR {
        lf("in {n}m", &[("n", &round(diff, MS_PER_MINUTE).to_string())])
    } else if diff < MS_PER_DAY {
        lf("in {n}h", &[("n", &round(diff, MS_PER_HOUR).to_string())])
    } else {
        lf("in {n}d", &[("n", &round(diff, MS_PER_DAY).to_string())])
    }
}

/// `describeTimeAgo`: "just now", "5m ago", "2h ago", "3d ago", then the local date
/// ("Jan 15", "1월 15일").
pub fn time_ago(iso: Option<&Value>, now: DateTime<Utc>) -> String {
    let Some(time) = parse_time(iso) else { return String::new() };
    let elapsed = (now - time).num_milliseconds();
    if elapsed < MS_PER_MINUTE {
        return l("just now").to_owned();
    }
    if elapsed < MS_PER_HOUR {
        return lf("{n}m ago", &[("n", &(elapsed / MS_PER_MINUTE).to_string())]);
    }
    if elapsed < MS_PER_DAY {
        return lf("{n}h ago", &[("n", &(elapsed / MS_PER_HOUR).to_string())]);
    }
    if elapsed < 7 * MS_PER_DAY {
        return lf("{n}d ago", &[("n", &(elapsed / MS_PER_DAY).to_string())]);
    }
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let local = time.with_timezone(&Local);
    let (month, number, day) = (MONTHS[local.month0() as usize], local.month().to_string(), local.day().to_string());
    ta("workspace.schedules.monthDay", &[("month", month), ("monthNumber", &number), ("day", &day)])
}

/// Cadence presets (`schedule-cadence-options.ts`): (id, English label, expression).
pub const CADENCE_PRESETS: [(&str, &str, &str); 5] = [
    ("every-minute", "Every minute", "* * * * *"),
    ("every-hour", "Every hour", "0 * * * *"),
    ("daily-9", "Daily 9:00", "0 9 * * *"),
    ("weekdays-9", "Weekdays 9:00", "0 9 * * 1-5"),
    ("mondays-9", "Mondays 9:00", "0 9 * * 1"),
];

/// `normalizeScheduleFormCadence`: interval cadences become the equivalent cron expression.
pub fn cron_expression_for(cadence: &Value) -> String {
    if str_of(cadence, "type") == "cron" {
        return str_of(cadence, "expression").to_owned();
    }
    let (value, unit) = every_ms_to_parts(cadence.get("everyMs").and_then(Value::as_f64));
    match unit {
        "minute" if value == 1 => "* * * * *".into(),
        "minute" => format!("*/{} * * * *", value.clamp(1, 59)),
        "hour" if value == 1 => "0 * * * *".into(),
        "hour" => format!("0 */{} * * *", value.clamp(1, 23)),
        _ if value == 1 => "0 9 * * *".into(),
        _ => format!("0 9 */{} * *", value.clamp(1, 31)),
    }
}

/// `parseMaxRuns` (`Number.parseInt`, positive only).
pub fn parse_max_runs(text: &str) -> Option<i64> {
    let trimmed = text.trim_start();
    let (sign, rest) = match trimmed.as_bytes().first() {
        Some(b'+') => (1, &trimmed[1..]),
        Some(b'-') => (-1, &trimmed[1..]),
        _ => (1, trimmed),
    };
    let end = rest.bytes().take_while(u8::is_ascii_digit).count();
    let value: i64 = rest[..end].parse().ok()?;
    let value = sign * value;
    (value > 0).then_some(value)
}

/// Schedule states (`schedule-derivation.ts` precedence).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleState {
    Expired,
    TargetGone,
    Finished,
    Paused,
    Active,
}

impl ScheduleState {
    pub fn runnable(self) -> bool {
        matches!(self, Self::Active | Self::Paused)
    }
}

/// Expired, target gone, finished, paused, active. A missing agent only counts as gone once
/// the host's agent directory has loaded.
pub fn schedule_state(schedule: &Value, host: Option<Host<'_>>, now: DateTime<Utc>) -> ScheduleState {
    if parse_time(schedule.get("expiresAt")).is_some_and(|expiry| expiry <= now) {
        return ScheduleState::Expired;
    }
    let target = object_of(schedule, "target");
    if str_of(target, "type") == "agent" && let Some(host) = host && host.agents_loaded() && host.agent(str_of(target, "agentId")).is_none() {
        return ScheduleState::TargetGone;
    }
    match str_of(schedule, "status") {
        "completed" => ScheduleState::Finished,
        "paused" => ScheduleState::Paused,
        _ => ScheduleState::Active,
    }
}

/// `resolveTarget`: agent title, project name, or shortened cwd, plus the provider.
pub fn target_label(schedule: &Value, host: Option<Host<'_>>) -> (String, Option<String>) {
    let target = object_of(schedule, "target");
    if str_of(target, "type") == "agent" {
        let Some(agent) = host.and_then(|h| h.agent(str_of(target, "agentId"))) else {
            return (l("Agent unavailable").into(), None);
        };
        let title = str_of(agent, "title").trim();
        let provider = str_of(agent, "provider");
        return (
            if title.is_empty() { l("Untitled agent").into() } else { title.into() },
            (!provider.is_empty()).then(|| provider.to_owned()),
        );
    }
    let config = object_of(target, "config");
    let cwd = str_of(config, "cwd").trim();
    let name = host.and_then(|h| h.project_by_root(cwd)).map(|p| str_of(p, "projectDisplayName").to_owned()).unwrap_or_default();
    let provider = str_of(config, "provider");
    (
        if name.is_empty() { shorten_path(cwd) } else { name },
        (!provider.is_empty()).then(|| provider.to_owned()),
    )
}

/// `buildMeta`: host (multi-host only), cadence, created, last run, next run while active.
pub fn meta(schedule: &Value, state: ScheduleState, host_label: Option<&str>, now: DateTime<Utc>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(label) = host_label {
        parts.push(label.to_owned());
    }
    parts.push(format_cadence(object_of(schedule, "cadence")));
    parts.push(lf("Created {time}", &[("time", &time_ago(schedule.get("createdAt"), now))]));
    parts.push(match schedule.get("lastRunAt").filter(|v| !v.is_null()) {
        None => l("Never run").to_owned(),
        Some(value) => lf("Last run {time}", &[("time", &time_ago(Some(value), now))]),
    });
    if state == ScheduleState::Active {
        let next = format_next_run(schedule.get("nextRunAt"), now);
        if !next.is_empty() {
            parts.push(lf("Next run {time}", &[("time", &next)]));
        }
    }
    parts.join(" · ")
}

/// The optimistic row after `pause`/`resume` (`useScheduleMutations`).
pub fn optimistic_status(schedule: &Value, action: &str, now: DateTime<Utc>) -> Value {
    let mut next = schedule.clone();
    let paused = action == "pause";
    next["status"] = json!(if paused { "paused" } else { "active" });
    next["pausedAt"] = if paused { json!(now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)) } else { Value::Null };
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins this test thread's catalog; the locale is thread-local, so parallel tests on
    /// other threads keep their own.
    fn pin(language: &str) {
        crate::common::i18n::set_language(Some(&json!(language)));
    }

    #[test]
    fn titles_and_paths() {
        assert_eq!(schedule_title(&json!({"name": " Daily "})), "Daily");
        assert_eq!(schedule_title(&json!({"target": {"type": "new-agent", "config": {"title": "T"}}})), "T");
        assert_eq!(schedule_title(&json!({"prompt": "\n  first\nsecond"})), "first");
        assert_eq!(shorten_path("/home/me/code"), "~/code");
        assert_eq!(shorten_path("/srv/x"), "/srv/x");
    }

    #[test]
    fn cadence_and_cron() {
        pin("en");
        assert_eq!(every_ms_to_parts(Some(7_200_000.0)), (2, "hour"));
        assert_eq!(every_ms_to_parts(Some(90_000.0)), (2, "minute"));
        assert_eq!(cron_expression_for(&json!({"type": "every", "everyMs": 3_600_000})), "0 * * * *");
        assert_eq!(cron_expression_for(&json!({"type": "every", "everyMs": 172_800_000})), "0 9 */2 * *");
        assert_eq!(describe_cron("0 9 * * 1-5", Some("UTC")).as_deref(), Some("Weekdays at 09:00 UTC"));
        assert_eq!(describe_cron("5 * * * *", None).as_deref(), Some("Every hour at :05"));
        assert_eq!(describe_cron("*/5 * * * *", None), None);
        assert_eq!(validate_cron("* * *").as_deref(), Some("Cron expressions must have 5 fields"));
        assert_eq!(validate_cron("60 * * * *").as_deref(), Some("Invalid minute value"));
        assert_eq!(validate_cron("*/0 * * * *").as_deref(), Some("Invalid minute step"));
        assert_eq!(validate_cron("1-70 * * * *").as_deref(), Some("Invalid minute range"));
        assert!(validate_cron("0 9 * * 1-5").is_none());
    }

    #[test]
    fn relative_times() {
        pin("en");
        let now = DateTime::parse_from_rfc3339("2026-01-10T12:00:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(time_ago(Some(&json!("2026-01-10T11:58:00Z")), now), "2m ago");
        assert_eq!(format_next_run(Some(&json!("2026-01-10T15:00:00Z")), now), "in 3h");
        assert_eq!(format_next_run(Some(&json!("2026-01-10T12:00:30Z")), now), "soon");
    }

    #[test]
    fn korean_literals() {
        pin("ko");
        let now = DateTime::parse_from_rfc3339("2026-01-10T12:00:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(describe_cron("0 9 * * 1-5", Some("UTC")).as_deref(), Some("평일 09:00 UTC"));
        assert_eq!(time_ago(Some(&json!("2026-01-10T11:58:00Z")), now), "2분 전");
        assert_eq!(validate_cron("60 * * * *").as_deref(), Some("잘못된 분 값"));
        assert_eq!(validate_cron("0 9 * 13 *").as_deref(), Some("잘못된 월 값"));
        assert_eq!(time_ago(Some(&json!("2025-12-01T12:00:00Z")), now), "12월 1일");
        pin("en");
        assert_eq!(time_ago(Some(&json!("2026-01-10T11:58:00Z")), now), "2m ago");
        assert_eq!(time_ago(Some(&json!("2025-12-01T12:00:00Z")), now), "Dec 1");
    }

    #[test]
    fn max_runs_parse_int_rules() {
        assert_eq!(parse_max_runs(" 12abc"), Some(12));
        assert_eq!(parse_max_runs("-3"), None);
        assert_eq!(parse_max_runs(""), None);
    }
}
