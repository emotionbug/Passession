//! Provider-neutral usage reports. Account keys must not contain credentials or raw email.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Tone {
    Default,
    Ok,
    Warning,
    Danger,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<bool>,
    pub used_pct: Option<f64>,
    pub remaining_pct: Option<f64>,
    pub resets_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runs_out_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shortfall_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tone: Option<Tone>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Balance {
    pub id: String,
    pub label: String,
    pub unit: Unit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tone: Option<Tone>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Unit {
    Usd,
    Credits,
    Requests,
    Tokens,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Detail {
    pub id: String,
    pub label: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tone: Option<Tone>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Problem {
    Expired {
        expires_at: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        refreshed_by: Option<String>,
    },
    Rejected {
        status: u16,
        #[serde(skip_serializing_if = "Option::is_none")]
        refreshed_by: Option<String>,
    },
    NoQuota {
        detail: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "status",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum Report {
    Available {
        #[serde(skip_serializing_if = "Option::is_none")]
        plan_label: Option<String>,
        windows: Vec<Window>,
        #[serde(skip_serializing_if = "Option::is_none")]
        balances: Option<Vec<Balance>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Vec<Detail>>,
    },
    Unavailable {
        problem: Problem,
    },
    Error {
        error: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    pub input: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Scope {
    Global,
    Session {
        provider: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        env: BTreeMap<String, String>,
    },
}

pub fn hash_account_key(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
pub fn tone_from_used_pct(value: Option<f64>) -> Tone {
    match value {
        None => Tone::Default,
        Some(v) if v > 90.0 => Tone::Danger,
        Some(v) if v >= 70.0 => Tone::Warning,
        _ => Tone::Ok,
    }
}
pub fn balance_tone_from_remaining(value: Option<f64>) -> Tone {
    match value {
        None => Tone::Default,
        Some(v) if v <= 0.0 => Tone::Danger,
        _ => Tone::Ok,
    }
}
pub fn used_pct_of(used: Option<f64>, limit: Option<f64>) -> Option<f64> {
    used.zip(limit)
        .filter(|(_, limit)| *limit > 0.0)
        .map(|(used, limit)| used / limit * 100.0)
}
pub fn window_from_used_pct(
    id: impl Into<String>,
    label: impl Into<String>,
    used_pct: Option<f64>,
    resets_at: Option<String>,
) -> Window {
    Window {
        id: id.into(),
        label: label.into(),
        short_label: None,
        summary: None,
        used_pct,
        remaining_pct: used_pct.map(|v| (100.0 - v).max(0.0)),
        resets_at,
        runs_out_at: None,
        shortfall_pct: None,
        tone: None,
    }
}
/// Explicit fallback names are used when the upstream does not report a duration.
/// Reset countdowns are not window durations. Optional scope is a stable quota ID and label.
pub fn window_from_reported_duration(
    duration_seconds: Option<f64>,
    scope: Option<(&str, &str)>,
    unknown: (&str, &str, &str),
    used_pct: Option<f64>,
    resets_at: Option<String>,
) -> Window {
    let (mut id, mut label, mut short): (String, String, String) =
        match duration_seconds.filter(|v| v.is_finite() && *v > 0.0) {
            Some(604800.0) => ("weekly".into(), "Weekly".into(), "wk".into()),
            Some(seconds) => {
                let id = if seconds == 18000.0 {
                    "five_hour".into()
                } else {
                    format!("{seconds}s")
                };
                let (size, name, short) = [
                    (86400.0, "day", "d"),
                    (3600.0, "hour", "h"),
                    (60.0, "minute", "m"),
                    (1.0, "second", "s"),
                ]
                .into_iter()
                .find(|(size, _, _)| seconds % size == 0.0)
                .unwrap_or((1.0, "second", "s"));
                let amount = seconds / size;
                (id, format!("{amount}-{name}"), format!("{amount}{short}"))
            }
            None => (unknown.0.into(), unknown.1.into(), unknown.2.into()),
        };
    if let Some((scope_id, scope_label)) = scope {
        id = format!("{scope_id}:{id}");
        label = format!("{scope_label} · {label}");
        short = if short.is_empty() {
            scope_label.into()
        } else {
            format!("{scope_label} {short}")
        };
    }
    let mut window = window_from_used_pct(id, label, used_pct, resets_at);
    window.short_label = Some(short);
    window
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duration_identity_and_usage_thresholds_match_existing_reports() {
        let window = window_from_reported_duration(
            Some(18000.0),
            Some(("model", "Model")),
            ("unknown", "Unknown", ""),
            Some(120.0),
            None,
        );
        assert_eq!(window.id, "model:five_hour");
        assert_eq!(window.label, "Model · 5-hour");
        assert_eq!(window.remaining_pct, Some(0.0));
        assert_eq!(
            window_from_reported_duration(Some(604800.0), None, ("x", "X", ""), None, None).id,
            "weekly"
        );
        assert_eq!(
            window_from_reported_duration(None, None, ("quota", "Quota", ""), None, None).id,
            "quota"
        );
        assert_eq!(tone_from_used_pct(Some(90.0)), Tone::Warning);
        assert_eq!(tone_from_used_pct(Some(90.1)), Tone::Danger);
        assert_eq!(used_pct_of(Some(50.0), Some(0.0)), None);
        assert_eq!(used_pct_of(Some(3.0), Some(4.0)), Some(75.0));
        assert_eq!(
            hash_account_key("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let report = Report::Unavailable {
            problem: Problem::NoQuota {
                detail: "No allocation".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(report).unwrap(),
            serde_json::json!({"status":"unavailable","problem":{"kind":"no_quota","detail":"No allocation"}})
        );
    }
}
