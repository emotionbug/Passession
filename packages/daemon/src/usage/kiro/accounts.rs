//! omp-kiro named account stores and the OMP model providers that pin their profiles.
use super::auth::{Credential, Profile};
use std::path::{Path, PathBuf};

pub(super) const DEFAULT_ACCOUNT: &str = "default";
const HTTP_REGIONS: [&str; 2] = ["us-east-1", "eu-central-1"];

/// 1–32 lowercase letters, digits or inner hyphens; `http` and `cli` name the unpinned providers.
fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=32).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
        && !matches!(name, DEFAULT_ACCOUNT | "http" | "cli")
}

/// The default store first, then `accounts/<name>.sqlite3` beside it, by name.
pub(super) fn stores(default: &Path) -> Vec<(String, PathBuf)> {
    let mut named: Vec<(String, PathBuf)> = default
        .parent()
        .and_then(|parent| std::fs::read_dir(parent.join("accounts")).ok())
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_file() {
                return None;
            }
            let file = entry.file_name();
            let name = file.to_str()?.strip_suffix(".sqlite3")?;
            valid_name(name).then(|| (name.to_owned(), entry.path()))
        })
        .collect();
    named.sort_unstable();
    let mut stores = Vec::with_capacity(named.len() + 1);
    stores.push((DEFAULT_ACCOUNT.to_owned(), default.to_owned()));
    stores.extend(named);
    stores
}

/// One account profile usable over Kiro HTTP.
pub(super) struct Target {
    pub provider: String,
    pub arn: String,
    pub name: Option<String>,
}

/// `kiro-<account>-<profile id>`: the ARN's final segment reduced to lowercase
/// ASCII letters and digits, exactly as omp-kiro names the provider.
pub(super) fn target_provider(account: &str, arn: &str) -> Option<String> {
    let (_, id) = arn.rsplit_once(":profile/")?;
    if id.contains(':') {
        return None;
    }
    let mut provider = format!("kiro-{account}-");
    let prefix = provider.len();
    provider.extend(
        id.chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase()),
    );
    (provider.len() > prefix).then_some(provider)
}

/// The listed catalog, or the saved selection when no catalog was listed yet.
pub(super) fn targets(
    account: &str,
    credential: &Credential,
    catalog: Option<Vec<Profile>>,
) -> Vec<Target> {
    let mut profiles = catalog.unwrap_or_else(|| {
        credential
            .profile_arn
            .iter()
            .map(|arn| Profile {
                arn: arn.clone(),
                name: None,
            })
            .collect()
    });
    profiles.retain(|profile| {
        profile
            .arn
            .split(':')
            .nth(3)
            .is_some_and(|region| HTTP_REGIONS.contains(&region))
    });
    profiles.sort_unstable_by(|left, right| left.arn.cmp(&right.arn));
    let mut targets: Vec<Target> = Vec::with_capacity(profiles.len());
    for profile in profiles {
        let Some(provider) = target_provider(account, &profile.arn) else {
            continue;
        };
        if targets.iter().any(|target| target.provider == provider) {
            continue;
        }
        targets.push(Target {
            provider,
            arn: profile.arn,
            name: profile.name,
        });
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_names_match_omp_kiro() {
        let arn = "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK";
        assert_eq!(
            target_provider("work", arn).as_deref(),
            Some("kiro-work-ehga3grvqmuk")
        );
        assert_eq!(
            target_provider(
                "a-b",
                "arn:aws:codewhisperer:eu-central-1:1:profile/Team-02"
            )
            .as_deref(),
            Some("kiro-a-b-team02")
        );
        for arn in [
            "arn:aws:codewhisperer:us-east-1:1:profile/--",
            "arn:aws:codewhisperer:us-east-1:1:other/X",
        ] {
            assert_eq!(target_provider("work", arn), None);
        }
        for name in ["work", "a", "a-1", &"a".repeat(32)] {
            assert!(valid_name(name), "{name}");
        }
        for name in [
            "",
            "Work",
            "-a",
            "a-",
            "default",
            "http",
            "cli",
            "a_b",
            &"a".repeat(33),
        ] {
            assert!(!valid_name(name), "{name}");
        }
    }
}
