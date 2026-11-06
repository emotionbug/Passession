//! Usage report rules from `usage_settings.dart` (original `usage/queries.ts`,
//! `usage/card.tsx`): report ordering, pins, legacy mapping and labels.

use serde_json::{Map, Value, json};

use super::time::{is_after, relative};
use crate::features::settings::i18n::{t, ta};
use crate::features::settings::json::{arr, get, obj, str_of};

/// Original `supportsUsageReports`.
pub fn supports_usage(server_info: &Value) -> bool {
    let features = obj(server_info, "features");
    features.get("usageSources").and_then(Value::as_bool) == Some(true)
        || features.get("providerUsageList").and_then(Value::as_bool) == Some(true)
}

/// Original `laterFetched`: a copy fetched after the arriving one stays.
fn later_fetched(shown: &Value, arriving: &Value) -> Value {
    if is_after(str_of(shown, "fetchedAt"), str_of(arriving, "fetchedAt")) { shown.clone() } else { arriving.clone() }
}

/// Original `upsertReport`.
pub fn upsert(reports: &mut Vec<Value>, report: Value) {
    match reports.iter_mut().find(|r| r.get("id") == report.get("id")) {
        Some(existing) => *existing = later_fetched(existing, &report),
        None => reports.push(report),
    }
}

/// Original `settleReports`: shown reports keep their place, missing ones go,
/// new ones are appended, newer copies stay.
pub fn settle(shown: Option<&[Value]>, finished: Vec<Value>) -> Vec<Value> {
    let mut remaining = finished;
    let mut settled = Vec::new();
    for report in shown.unwrap_or(&[]) {
        if let Some(index) = remaining.iter().position(|r| r.get("id") == report.get("id")) {
            let arriving = remaining.remove(index);
            settled.push(later_fetched(report, &arriving));
        }
    }
    settled.extend(remaining);
    settled
}

/// Original `replaceReport`: `None` means the host no longer knows the report.
pub fn replace(reports: &mut Vec<Value>, report_id: &str, refreshed: Option<Value>) {
    match refreshed {
        None => reports.retain(|r| str_of(r, "id") != report_id),
        Some(next) => {
            for report in reports.iter_mut() {
                if str_of(report, "id") == report_id {
                    *report = next.clone();
                }
            }
        }
    }
}

/// Original `displayPercent`.
pub fn display_percent(window: &Value, remaining: bool) -> Option<f64> {
    let used = window.get("usedPct").and_then(Value::as_f64);
    let left = window.get("remainingPct").and_then(Value::as_f64);
    let used_pct = used.or(left.map(|l| 100.0 - l));
    if !remaining {
        return used_pct;
    }
    left.or(used_pct.map(|u| 100.0 - u))
}

fn pin(source: &Value, window: &Value) -> Value {
    json!({ "sourceId": source, "windowId": window })
}

/// Original `effectiveUsagePins`.
pub fn effective_pins(preferences: &Value, reports: &[Value]) -> Vec<Value> {
    if let Some(Value::Array(pins)) = preferences.get("pins") {
        return pins.clone();
    }
    let remaining = str_of(preferences, "displayAs") == "remaining";
    let mut pins: Vec<Value> = Vec::new();
    for entry in reports {
        let report = get(entry, "report");
        if str_of(report, "status") != "available" {
            continue;
        }
        let with_percent: Vec<&Value> = arr(report, "windows").iter().filter(|w| display_percent(w, remaining).is_some()).collect();
        let marked: Vec<&Value> = with_percent.iter().copied().filter(|w| w.get("summary").and_then(Value::as_bool) == Some(true)).collect();
        let chosen: Vec<&Value> = if marked.is_empty() { with_percent.into_iter().take(1).collect() } else { marked };
        for window in chosen {
            let candidate = pin(get(entry, "sourceId"), get(window, "id"));
            if !pins.contains(&candidate) {
                pins.push(candidate);
            }
        }
    }
    pins
}

/// Original `toggleUsagePin`: the first edit snapshots the shown defaults.
pub fn toggle_pin(preferences: &Value, source_id: &Value, window_id: &Value, reports: &[Value]) -> Value {
    let target = pin(source_id, window_id);
    let selected = effective_pins(preferences, reports);
    let next: Vec<Value> = if selected.contains(&target) {
        selected.into_iter().filter(|p| *p != target).collect()
    } else {
        let mut list = selected;
        list.push(target);
        list
    };
    let mut out = preferences.as_object().cloned().unwrap_or_default();
    out.insert("pins".into(), Value::Array(next));
    Value::Object(out)
}

/// COMPAT(providerUsageList): a pre-usageSources host's provider list as
/// report entries (original `listUsageReports`).
pub fn legacy_reports(payload: &Value) -> Vec<Value> {
    arr(payload, "providers")
        .iter()
        .map(|provider| {
            let report = match str_of(provider, "status") {
                "available" => {
                    let mut report = Map::new();
                    report.insert("status".into(), json!("available"));
                    report.insert("windows".into(), Value::Array(arr(provider, "windows").to_vec()));
                    for key in ["balances", "details", "planLabel"] {
                        if let Some(value) = provider.get(key).filter(|v| !v.is_null()) {
                            report.insert(key.into(), value.clone());
                        }
                    }
                    Value::Object(report)
                }
                "error" => json!({"status": "error", "error": str_of(provider, "error")}),
                _ => json!({"status": "unavailable", "problem": {"kind": "no_quota", "detail": str_of(provider, "error")}}),
            };
            json!({
                "id": get(provider, "providerId"),
                "sourceId": get(provider, "providerId"),
                "sourceLabel": get(provider, "displayName"),
                "account": {},
                "fetchedAt": provider.get("fetchedAt").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| get(payload, "fetchedAt").clone()),
                "report": report,
            })
        })
        .collect()
}

/// Original `formatProviderUsageLabel`: the daemon's English labels for the
/// standard windows are translated; any other label shows as sent.
pub fn usage_label(id: &str, label: &str) -> String {
    const REVIEW: &str = "Code review · ";
    if let (Some(rest_id), Some(rest_label)) = (id.strip_prefix("code_review:"), label.strip_prefix(REVIEW)) {
        return format!("{} · {}", t("providerUsage.labels.codeReview"), usage_label(rest_id, rest_label));
    }
    let standard = match id {
        "session" | "five_hour" => Some(("Session", "session")),
        "weekly" => Some(("Weekly", "weekly")),
        "monthly" => Some(("Monthly", "monthly")),
        "code_review" => Some(("Code review", "codeReview")),
        "credits" => Some(("Credits", "credits")),
        "monthly_credits" => Some(("Monthly credits", "monthlyCredits")),
        _ => None,
    };
    match standard {
        Some((english, key)) if english == label => t(&format!("providerUsage.labels.{key}")),
        _ => label.to_string(),
    }
}

/// Original `usageCopy.problem` / `usageProblemText`.
pub fn problem_text(report: &Value) -> String {
    match str_of(report, "status") {
        "error" => return str_of(report, "error").to_string(),
        "unavailable" => {}
        _ => return String::new(),
    }
    let problem = get(report, "problem");
    if str_of(problem, "kind") == "no_quota" {
        return str_of(problem, "detail").to_string();
    }
    let refreshed_by = str_of(problem, "refreshedBy");
    let remedy = if refreshed_by.is_empty() {
        t("providerUsage.signInAgain")
    } else {
        ta("providerUsage.runToRefresh", &[("command", refreshed_by)])
    };
    if str_of(problem, "kind") == "rejected" {
        let status = crate::features::settings::json::scalar_text(get(problem, "status"));
        return ta("providerUsage.loginRejected", &[("status", &status), ("remedy", &remedy)]);
    }
    let when = relative(problem.get("expiresAt").and_then(Value::as_str), true).unwrap_or_default();
    ta("providerUsage.loginExpired", &[("relativeTime", &when), ("remedy", &remedy)])
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UsageView {
    Unavailable,
    Loading,
    Error,
    Ready,
}

/// Original `resolveUsageView`.
pub fn resolve_view(connected: bool, supported: bool, has_data: bool, has_error: bool) -> UsageView {
    if !connected || !supported {
        UsageView::Unavailable
    } else if has_data {
        UsageView::Ready
    } else if has_error {
        UsageView::Error
    } else {
        UsageView::Loading
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settle_keeps_shown_order_drops_missing_and_keeps_newer_copies() {
        let shown = vec![
            json!({"id": "a", "fetchedAt": "2026-01-01T00:00:05Z", "v": 1}),
            json!({"id": "b", "fetchedAt": "2026-01-01T00:00:00Z"}),
        ];
        let finished = vec![json!({"id": "c"}), json!({"id": "a", "fetchedAt": "2026-01-01T00:00:01Z", "v": 2})];
        let settled = settle(Some(&shown), finished);
        assert_eq!(settled.iter().map(|r| str_of(r, "id")).collect::<Vec<_>>(), vec!["a", "c"]);
        assert_eq!(settled[0]["v"], json!(1), "the later-fetched shown copy stays");
    }

    #[test]
    fn default_pins_prefer_summary_windows_and_first_toggle_snapshots_defaults() {
        let reports = vec![json!({
            "sourceId": "claude",
            "report": {"status": "available", "windows": [
                {"id": "session", "usedPct": 10},
                {"id": "weekly", "usedPct": 20, "summary": true}
            ]}
        })];
        let preferences = json!({"displayAs": "used"});
        assert_eq!(effective_pins(&preferences, &reports), vec![json!({"sourceId": "claude", "windowId": "weekly"})]);
        let next = toggle_pin(&preferences, &json!("claude"), &json!("session"), &reports);
        assert_eq!(next["pins"], json!([{"sourceId": "claude", "windowId": "weekly"}, {"sourceId": "claude", "windowId": "session"}]));
        assert_eq!(next["displayAs"], json!("used"));
    }

    #[test]
    fn remaining_display_derives_from_used() {
        assert_eq!(display_percent(&json!({"usedPct": 30.0}), true), Some(70.0));
        assert_eq!(display_percent(&json!({"remainingPct": 25.0}), false), Some(75.0));
        assert_eq!(display_percent(&json!({}), false), None);
    }
}
