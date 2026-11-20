use super::super::common::{hash_account_key, tone, unavailable, used_pct, window};
use super::native_provider;
use chrono::{DateTime, SecondsFormat};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Usage {
    reports: Vec<Report>,
    #[serde(default)]
    accounts_without_usage: Vec<Credential>,
    #[serde(default)]
    disabled_credentials: Vec<Credential>,
}

#[derive(Deserialize)]
pub(super) struct Accounts {
    accounts: Vec<OAuthAccount>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Identity {
    email: Option<String>,
    account_id: Option<String>,
    project_id: Option<String>,
    enterprise_url: Option<String>,
    org_id: Option<String>,
    org_name: Option<String>,
}

impl Identity {
    fn key(&self, provider: &str) -> Option<String> {
        let scoped = matches!(provider, "anthropic" | "openai-codex");
        let email = self
            .email
            .as_deref()
            .and_then(nonempty)
            .map(|email| ("email", email));
        let account = self
            .account_id
            .as_deref()
            .and_then(nonempty)
            .map(|id| ("account", id));
        let project = self
            .project_id
            .as_deref()
            .and_then(nonempty)
            .map(|id| ("project", id));
        let base = if scoped {
            email.or(account)
        } else {
            account.or(email)
        }
        .or(project);
        let org = self.org_id.as_deref().and_then(nonempty);
        match (base, org) {
            (Some((kind, id)), Some(org)) if scoped => Some(format!(
                "{kind}:{}|org:{}",
                id.to_lowercase(),
                org.to_lowercase()
            )),
            (Some((kind, id)), _) => Some(format!("{kind}:{}", id.to_lowercase())),
            (None, Some(org)) => Some(format!("org:{}", org.to_lowercase())),
            (None, None) => None,
        }
    }

    fn label(&self) -> Option<String> {
        let enterprise = self
            .enterprise_url
            .as_deref()
            .and_then(|url| reqwest::Url::parse(url).ok());
        let base = [&self.email, &self.account_id, &self.project_id]
            .into_iter()
            .find_map(|value| value.as_deref().and_then(nonempty))
            .or_else(|| enterprise.as_ref().and_then(reqwest::Url::host_str));
        let org = self
            .org_name
            .as_deref()
            .and_then(nonempty)
            .or_else(|| self.org_id.as_deref().and_then(nonempty));
        label(base, org)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Credential {
    provider: String,
    #[serde(rename = "type")]
    kind: String,
    id: Option<u64>,
    #[serde(flatten)]
    identity: Identity,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OAuthAccount {
    provider: String,
    identity_key: Option<String>,
    org_name: Option<String>,
}

#[derive(Deserialize)]
struct Report {
    provider: String,
    limits: Vec<Limit>,
    metadata: Option<Metadata>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    #[serde(flatten)]
    identity: Identity,
    credential_id: Option<u64>,
    subscription_plan: Option<String>,
    plan_type: Option<String>,
}

#[derive(Deserialize)]
struct Limit {
    id: String,
    label: String,
    scope: Identity,
    window: Option<Window>,
    amount: Amount,
    status: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Window {
    resets_at: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Amount {
    used: Option<f64>,
    limit: Option<f64>,
    remaining: Option<f64>,
    used_fraction: Option<f64>,
    remaining_fraction: Option<f64>,
    unit: String,
}

impl Amount {
    fn percent(&self) -> Option<f64> {
        self.used_fraction
            .map(|used| used * 100.0)
            .or_else(|| used_pct(self.used, self.limit))
            .or_else(|| (self.unit == "percent").then_some(self.used).flatten())
            .or_else(|| {
                self.remaining_fraction
                    .map(|remaining| ((1.0 - remaining) * 100.0).max(0.0))
            })
            .filter(|value| value.is_finite())
    }
}

pub(super) fn no_quota(detail: &str) -> Value {
    unavailable(json!({"kind":"no_quota","detail":detail}))
}

const NO_QUOTA: &str = "OMP reported no usage for this account.";

fn nonempty(text: &str) -> Option<&str> {
    let text = text.trim();
    (!text.is_empty()).then_some(text)
}

fn display_text(text: &str) -> String {
    text.replace(char::is_control, " ")
}

fn label(base: Option<&str>, org: Option<&str>) -> Option<String> {
    match (base, org) {
        (Some(base), Some(org)) if base != org => {
            Some(format!("{} · {}", display_text(base), display_text(org)))
        }
        (Some(text), _) | (_, Some(text)) => Some(display_text(text)),
        (None, None) => None,
    }
}

fn project_report(limits: Vec<Limit>, plan: Option<&str>) -> Value {
    let mut windows = Vec::new();
    let mut balances = Vec::new();
    let mut details = Vec::new();
    for limit in limits {
        let resets_at = limit
            .window
            .as_ref()
            .and_then(|window| window.resets_at)
            .and_then(|at| DateTime::from_timestamp_millis(at.trunc() as i64))
            .map(|at| at.to_rfc3339_opts(SecondsFormat::Millis, true));
        let pct = limit.amount.percent();
        let tone = match limit.status.as_deref() {
            Some("exhausted") => "danger",
            Some("warning") => "warning",
            _ => tone(pct),
        };
        let id = display_text(&limit.id);
        let label = display_text(&limit.label);
        let amount = limit.amount;
        let remaining = amount.remaining.or_else(|| {
            let (used, limit) = amount.used.zip(amount.limit)?;
            let remaining = (limit - used).max(0.0);
            remaining.is_finite().then_some(remaining)
        });
        if pct.is_some() || limit.window.is_some() {
            windows.push(window(
                &id,
                &label,
                None,
                pct,
                resets_at.as_deref(),
                windows.is_empty(),
                Some(tone),
            ));
        }
        if amount.used.is_none() && amount.limit.is_none() && remaining.is_none() {
            continue;
        }
        match amount.unit.as_str() {
            "usd" | "credits" | "requests" | "tokens" => balances.push(json!({
                "id": id, "label": label, "used": amount.used, "limit": amount.limit, "remaining": remaining,
                "unit": amount.unit, "resetsAt": resets_at, "tone": tone,
            })),
            "percent" => {}
            unit => {
                // The Passession balance schema has a closed unit enum. Keep
                // other OMP units visible without relabeling them as credits.
                let unit = match unit { "bytes" => " bytes", "minutes" => " min", _ => "" };
                let parts: Vec<_> = [("used", amount.used), ("limit", amount.limit), ("left", remaining)]
                    .into_iter().filter_map(|(label, value)| value.map(|value| format!("{value}{unit} {label}"))).collect();
                details.push(json!({"id":id,"label":label,"value":parts.join(" · "),"tone":tone}));
            }
        }
    }
    if windows.is_empty() && balances.is_empty() && details.is_empty() {
        return no_quota(NO_QUOTA);
    }
    let mut report = json!({"status":"available","windows":windows});
    if let Some(plan) = plan {
        report["planLabel"] = json!(display_text(plan));
    }
    if !balances.is_empty() {
        report["balances"] = json!(balances);
    }
    if !details.is_empty() {
        report["details"] = json!(details);
    }
    report
}

pub(super) struct Row {
    pub label: String,
    pub report: Value,
}

pub(super) type Rows = BTreeMap<String, Row>;

fn identity_hash(provider: &str, identity: &str) -> String {
    hash_account_key(&json!([provider, "identity", identity]).to_string())
}

pub(super) fn project(
    usage: Usage,
    accounts: Option<Accounts>,
    namespace: &str,
    scope: Option<&str>,
) -> Rows {
    let included =
        |provider: &str| native_provider(provider) && scope.is_none_or(|scope| scope == provider);
    let mut rows = Rows::new();
    let mut anonymous = HashMap::<String, usize>::new();
    let mut stored = HashSet::new();
    let mut stored_anonymous = HashSet::new();
    if let Some(accounts) = accounts.as_ref() {
        for account in &accounts.accounts {
            if let Some(identity) = account.identity_key.as_deref().and_then(nonempty) {
                stored.insert(identity_hash(&account.provider, &identity.to_lowercase()));
            } else {
                stored_anonymous.insert(account.provider.as_str());
            }
        }
    }
    let mut add = |provider: &str,
                   identity: Option<String>,
                   id: Option<u64>,
                   label: Option<String>,
                   report: Value| {
        let (key, label) = if let Some(identity) = identity {
            let key = identity_hash(provider, &identity);
            (key, label.unwrap_or_else(|| "Account".into()))
        } else {
            let ordinal = anonymous.entry(provider.to_owned()).or_default();
            *ordinal += 1;
            let key = if let Some(id) = id {
                json!([namespace, provider, "credential", id])
            } else {
                json!([namespace, provider, "ordinal", *ordinal])
            };
            (
                hash_account_key(&key.to_string()),
                label.unwrap_or_else(|| format!("Account {ordinal}")),
            )
        };
        rows.insert(
            key,
            Row {
                label: format!("OMP · {provider} · {label}"),
                report,
            },
        );
    };
    for report in usage
        .reports
        .into_iter()
        .filter(|row| included(&row.provider))
    {
        let meta = report.metadata.unwrap_or_default();
        let identity = meta.identity.key(&report.provider).or_else(|| {
            report
                .limits
                .iter()
                .find_map(|limit| limit.scope.key(&report.provider))
        });
        // Tagged reports came from a stored OMP credential. For untagged
        // broker aggregates, use the OAuth inventory when available rather
        // than counting every matching env/runtime key as a stored account.
        if meta.credential_id.is_none()
            && accounts.is_some()
            && !identity.as_deref().map_or_else(
                || stored_anonymous.contains(report.provider.as_str()),
                |identity| stored.contains(&identity_hash(&report.provider, identity)),
            )
        {
            continue;
        }
        let label = meta
            .identity
            .label()
            .or_else(|| report.limits.iter().find_map(|limit| limit.scope.label()));
        let plan = meta
            .subscription_plan
            .as_deref()
            .and_then(nonempty)
            .or_else(|| meta.plan_type.as_deref().and_then(nonempty));
        let quota = project_report(report.limits, plan);
        add(&report.provider, identity, meta.credential_id, label, quota);
    }
    for (credentials, detail) in [
        (usage.accounts_without_usage, NO_QUOTA),
        (
            usage.disabled_credentials,
            "This OMP login is disabled. Sign in again using /login in OMP.",
        ),
    ] {
        for credential in credentials
            .into_iter()
            .filter(|row| included(&row.provider))
        {
            let label = credential
                .identity
                .label()
                .or_else(|| (credential.kind == "api_key").then(|| "API key".into()));
            add(
                &credential.provider,
                credential.identity.key(&credential.provider),
                credential.id,
                label,
                no_quota(detail),
            );
        }
    }
    if let Some(accounts) = accounts {
        // The unscoped usage command deliberately omits providers without a
        // usage endpoint. Supplement it with the separate token-free OAuth list.
        let mut seen = HashMap::<String, usize>::new();
        for account in accounts
            .accounts
            .into_iter()
            .filter(|row| included(&row.provider))
        {
            let identity = account.identity_key.as_deref().and_then(nonempty);
            let ordinal = seen.entry(account.provider.clone()).or_default();
            *ordinal += 1;
            let (key, base, org) = if let Some(identity) = identity {
                let (base, org) = identity
                    .split_once("|org:")
                    .map_or((identity, None), |(base, org)| (base, Some(org)));
                let label_base = base
                    .strip_prefix("email:")
                    .or_else(|| base.strip_prefix("account:"))
                    .or_else(|| base.strip_prefix("project:"))
                    .or_else(|| base.strip_prefix("org:"));
                let key = identity_hash(&account.provider, &identity.to_lowercase());
                (key, label_base, org)
            } else {
                if let Some(count) = anonymous
                    .get_mut(&account.provider)
                    .filter(|count| **count > 0)
                {
                    *count -= 1;
                    continue;
                }
                (
                    hash_account_key(
                        &json!([namespace, account.provider, "inventory", *ordinal]).to_string(),
                    ),
                    None,
                    None,
                )
            };
            let label = label(base, account.org_name.as_deref().and_then(nonempty).or(org))
                .unwrap_or_else(|| format!("OAuth account {ordinal}"));
            rows.entry(key).or_insert_with(|| Row {
                label: format!("OMP · {} · {label}", account.provider),
                report: no_quota(NO_QUOTA),
            });
        }
    }
    rows
}
