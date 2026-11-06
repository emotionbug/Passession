//! Locale-aware formatting for stream metadata (en / ko): message timestamps
//! (message_actions.dart formatMessageTimestamp), turn durations
//! (_ElapsedLabel.formatDuration) and overview group summaries.

use super::i18n::{t, ta};
use super::tools::OverviewSummary;
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};

fn korean() -> bool {
    crate::common::locale_tag() == "ko"
}

fn time_of_day(local: &DateTime<Local>) -> String {
    let (pm, hour) = local.hour12();
    if korean() {
        format!("{} {}:{:02}", if pm { "오후" } else { "오전" }, hour, local.minute())
    } else {
        format!("{}:{:02} {}", hour, local.minute(), if pm { "PM" } else { "AM" })
    }
}

/// Today: the time; the last week: weekday + time; older: date + time.
pub fn message_timestamp(millis: i64) -> String {
    let Some(local) = Local.timestamp_millis_opt(millis).single() else { return String::new() };
    let time = time_of_day(&local);
    let today = Local::now().date_naive();
    let days = (today - local.date_naive()).num_days();
    if days == 0 {
        return time;
    }
    let ko = korean();
    if days > 0 && days < 7 {
        let weekday = local.weekday().num_days_from_monday() as usize;
        let name = if ko {
            ["월요일", "화요일", "수요일", "목요일", "금요일", "토요일", "일요일"][weekday]
        } else {
            ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"][weekday]
        };
        return format!("{name} {time}");
    }
    if ko {
        format!("{}. {}. {}., {time}", local.year(), local.month(), local.day())
    } else {
        let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][local.month0() as usize];
        format!("{month} {}, {}, {time}", local.day(), local.year())
    }
}

pub fn duration(millis: i64) -> String {
    let total = (millis.max(0) / 1000) as u64;
    if total < 60 {
        return format!("{total}s");
    }
    let minutes = total / 60;
    if minutes < 60 {
        let seconds = total % 60;
        return if seconds == 0 { format!("{minutes}m") } else { format!("{minutes}m {seconds}s") };
    }
    let hours = minutes / 60;
    let rest = minutes % 60;
    if rest == 0 { format!("{hours}h") } else { format!("{hours}h {rest}m") }
}

/// _OverviewGroup.summaryLabel ("Edited 2 files and ran 3 commands").
pub fn overview_summary(summary: &OverviewSummary) -> String {
    let mut parts = Vec::new();
    for (count, key) in [
        (summary.edited_file_count, "toolCallGroup.editedFiles"),
        (summary.command_count, "toolCallGroup.commands"),
        (summary.read_file_count, "toolCallGroup.readFiles"),
        (summary.search_count, "toolCallGroup.searches"),
        (summary.other_tool_count, "toolCallGroup.otherTools"),
        (summary.paseo_call_count, "toolCallGroup.paseoCalls"),
    ] {
        if count > 0 {
            let suffix = if count == 1 { "one" } else { "other" };
            parts.push(ta(&format!("{key}.{suffix}"), &[("count", &count.to_string())]));
        }
    }
    if parts.is_empty() {
        return String::new();
    }
    let and = t("toolCallGroup.and");
    let joined = match parts.len() {
        1 => parts[0].clone(),
        2 => format!("{} {and} {}", parts[0], parts[1]),
        n => format!("{}, {and} {}", parts[..n - 1].join(", "), parts[n - 1]),
    };
    let mut chars = joined.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// message-compaction-label.ts getCompactionMarkerLabel.
pub fn compaction_label(status: &str, trigger: Option<&str>, pre_tokens: Option<f64>) -> String {
    if status == "loading" {
        return t("message.compaction.loading");
    }
    match trigger {
        Some("auto") => t("message.compaction.auto"),
        Some("manual") => t("message.compaction.manual"),
        _ => match pre_tokens {
            Some(tokens) if tokens != 0.0 => ta("message.compaction.withTokens", &[("tokens", &((tokens / 1000.0).round() as i64).to_string())]),
            _ => t("message.compaction.completed"),
        },
    }
}
