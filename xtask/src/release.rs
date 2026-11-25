//! Native release operations. Network mutations are explicit `publish`, `notes --publish`,
//! and `rollout` commands; plan/record/verify/manifests/stamp never publish.
use crate::common;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{env, path::PathBuf};
pub(crate) mod integrity;
mod metadata;
pub(crate) mod remote;

pub(super) const REPOSITORY: &str = "emotionbug/Passession";
pub(super) const WORKFLOW: &str = ".github/workflows/desktop-release.yml";
/// Desktop application feed runtime and asset prefix (`app-<channel>-<target>.json`).
pub(super) const CLIENT: &str = "app";
pub(super) const CLIENT_APP_ID: &str = "sh.paseo.sessions.desktop";
/// Standalone daemon feed runtime (`daemon-<channel>-<target>.json`).
pub(super) const DAEMON: &str = "rust";
#[derive(Clone, Copy, Debug, Serialize)]
pub(super) struct Target {
    #[serde(rename = "runner")]
    pub compile_runner: &'static str,
    pub finalizer: Option<&'static str>,
    pub platform: &'static str,
    pub arch: &'static str,
    pub desktop: bool,
    pub format: &'static str,
}
impl Target {
    pub fn id(self) -> String {
        format!("{}-{}", self.platform, self.arch)
    }
    fn matrix_entry(self) -> Result<Value> {
        let mut entry = serde_json::to_value(self)?;
        let id = self.id();
        entry["triple"] = json!(common::BuildTarget::parse(&id)?.triple);
        entry["id"] = json!(id);
        Ok(entry)
    }
}
pub(super) const TARGETS: [Target; 8] = [
    Target {
        compile_runner: "ubuntu-24.04",
        finalizer: None,
        platform: "linux",
        arch: "x64",
        desktop: true,
        format: "",
    },
    Target {
        compile_runner: "ubuntu-24.04-arm",
        finalizer: None,
        platform: "linux",
        arch: "arm64",
        desktop: true,
        format: "",
    },
    Target {
        compile_runner: "ubuntu-24.04",
        finalizer: Some("macos-15"),
        platform: "macos",
        arch: "arm64",
        desktop: true,
        format: "",
    },
    Target {
        compile_runner: "macos-15-intel",
        finalizer: None,
        platform: "macos",
        arch: "x64",
        desktop: true,
        format: "",
    },
    Target {
        compile_runner: "ubuntu-24.04",
        finalizer: Some("windows-latest"),
        platform: "windows",
        arch: "x64",
        desktop: true,
        format: "",
    },
    Target {
        compile_runner: "windows-11-arm",
        finalizer: None,
        platform: "windows",
        arch: "arm64",
        desktop: true,
        format: "",
    },
    Target {
        compile_runner: "ubuntu-24.04",
        finalizer: None,
        platform: "android",
        arch: "arm64",
        desktop: false,
        format: "apk",
    },
    Target {
        compile_runner: "macos-15",
        finalizer: None,
        platform: "ios",
        arch: "arm64",
        desktop: false,
        format: "",
    },
];
pub(super) fn target(id: &str) -> Result<Target> {
    TARGETS
        .iter()
        .copied()
        .find(|t| t.id() == id)
        .context("Unknown release target")
}

fn finalize_matrix(entries: &[Value]) -> Value {
    let include: Vec<_> = entries
        .iter()
        .filter(|entry| entry["finalizer"].is_string())
        .map(|entry| {
            let mut entry = entry.clone();
            entry["runner"] = entry["finalizer"].clone();
            entry
        })
        .collect();
    json!({"include": include})
}
pub(super) fn var(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.into())
}
pub(super) fn required(key: &str) -> Result<String> {
    let v = var(key, "");
    ensure!(!v.is_empty(), "Missing {key}");
    Ok(v)
}
pub(super) fn numeric_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with('0') && id.bytes().all(|b| b.is_ascii_digit())
}
pub(super) fn valid_sha(sha: &str) -> bool {
    sha.len() == 40
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(super) fn hours(raw: &str) -> Result<f64> {
    let n: f64 = raw.parse().context("rollout_hours must be nonnegative")?;
    ensure!(
        n.is_finite() && n >= 0.0,
        "rollout_hours must be nonnegative"
    );
    Ok(n)
}
pub(super) fn directory() -> Result<PathBuf> {
    Ok(common::root()?.join(var("RELEASE_DIRECTORY", "artifacts/native")))
}
pub(super) fn version(raw: &str) -> Result<semver::Version> {
    let v = semver::Version::parse(raw)?;
    ensure!(
        v.build.is_empty(),
        "Build metadata is not a release version"
    );
    if !v.pre.is_empty() {
        let n = v
            .pre
            .as_str()
            .strip_prefix("beta.")
            .context("Only beta.N prereleases are supported")?;
        ensure!(
            !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
            "Invalid beta number"
        );
    }
    Ok(v)
}
pub(super) fn tag_version(tag: &str) -> Result<semver::Version> {
    let tag = tag.strip_prefix("refs/tags/").unwrap_or(tag);
    for prefix in [
        "desktop-windows-v",
        "desktop-linux-v",
        "desktop-macos-v",
        "desktop-v",
        "android-v",
        "app-v",
        "v",
    ] {
        if let Some(v) = tag.strip_prefix(prefix) {
            return version(v);
        }
    }
    bail!("Unsupported source tag {tag}")
}
pub(super) fn channel(v: &semver::Version) -> &'static str {
    if v.pre.is_empty() { "stable" } else { "beta" }
}

/// Linux/Windows x64, Android and Apple Silicon macOS in one collection.
const X64_ANDROID_MACOS_ARM64: &str = "linux-windows-x64-android-macos-arm64";

fn plan(
    source: &str,
    selection: &str,
    mode: &str,
    publish: bool,
    recovery_run: &str,
    event: &str,
) -> Result<Value> {
    let v = tag_version(source)?;
    ensure!(
        ["build", "publish-only"].contains(&mode),
        "Unknown release mode"
    );
    ensure!(
        [
            "all",
            "desktop",
            "linux",
            "macos",
            "windows",
            "android",
            "ios",
            "linux-windows-x64-android",
            X64_ANDROID_MACOS_ARM64,
        ]
        .contains(&selection)
            || TARGETS.iter().any(|t| t.id() == selection),
        "Unknown platform selection"
    );
    ensure!(
        mode != "publish-only"
            || (["all", "linux-windows-x64-android", X64_ANDROID_MACOS_ARM64].contains(&selection)
                && publish),
        "publish-only requires platform=all, linux-windows-x64-android or {X64_ANDROID_MACOS_ARM64} and publish=true"
    );
    let selection = if event == "push" || (mode == "publish-only" && selection == "all") {
        if source.starts_with("desktop-macos-") {
            "macos"
        } else if source.starts_with("desktop-linux-") {
            "linux"
        } else if source.starts_with("desktop-windows-") {
            "windows"
        } else if source.starts_with("desktop-") {
            "desktop"
        } else if source.starts_with("android-") {
            "android"
        } else {
            "all"
        }
    } else {
        selection
    };
    let selected: Vec<_> = TARGETS
        .iter()
        .filter(|t| {
            selection == "all"
                || selection == t.platform
                || selection == t.id()
                || (selection == "desktop" && t.desktop)
                || (["linux-windows-x64-android", X64_ANDROID_MACOS_ARM64].contains(&selection)
                    && ((t.arch == "x64" && ["linux", "windows"].contains(&t.platform))
                        || t.platform == "android"))
                || (selection == X64_ANDROID_MACOS_ARM64 && t.id() == "macos-arm64")
        })
        .collect();
    let mut recovery = Vec::new();
    if mode == "publish-only" {
        recovery.extend(selected.iter().map(|t| t.id()));
    } else if publish && selected.len() == 1 && selected[0].desktop {
        recovery.extend(
            TARGETS
                .iter()
                .filter(|t| t.platform == selected[0].platform && t.id() != selected[0].id())
                .map(|t| t.id()),
        );
    }
    ensure!(
        recovery.is_empty() || numeric_id(recovery_run),
        "An exact prior recovery_run_id is required"
    );
    ensure!(
        !recovery.is_empty() || recovery_run.is_empty(),
        "recovery_run_id requires publish-only or a publishing single-target recovery"
    );
    let expected: Vec<_> = if mode == "publish-only" {
        recovery.clone()
    } else {
        selected
            .iter()
            .map(|t| t.id())
            .chain(recovery.iter().cloned())
            .collect()
    };
    let entries = selected
        .iter()
        .map(|target| target.matrix_entry())
        .collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"sourceTag":source,"releaseTag":format!("v{v}"),"version":v.to_string(),"channel":channel(&v),"prerelease":!v.pre.is_empty(),"publish":publish,"mode":mode,"finalize":finalize_matrix(&entries),"matrix":{"include":entries},"recovery":recovery,"expected":expected,"createRelease":(["all","desktop","android","linux-windows-x64-android",X64_ANDROID_MACOS_ARM64].contains(&selection))}),
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Identity {
    pub sha: String,
    pub source_tag: String,
    pub version: String,
    pub channel: String,
    pub run_id: String,
    pub target: String,
}
impl Identity {
    fn environment(id: &str, run: &str) -> Result<Self> {
        let result = Self {
            sha: required("SOURCE_SHA")?,
            source_tag: required("SOURCE_TAG")?,
            version: required("RELEASE_VERSION")?,
            channel: required("PASEO_RELEASE_CHANNEL")?,
            run_id: run.into(),
            target: id.into(),
        };
        ensure!(
            valid_sha(&result.sha) && numeric_id(run),
            "Missing exact source/run identity"
        );
        let v = tag_version(&result.source_tag)?;
        ensure!(
            v.to_string() == result.version && channel(&v) == result.channel,
            "Release identity disagrees with source tag"
        );
        target(id)?;
        Ok(result)
    }
}
pub(super) fn expected() -> Result<Vec<Identity>> {
    let targets: Vec<String> = serde_json::from_str(&required("EXPECTED_TARGETS")?)?;
    let recovered: Vec<String> = serde_json::from_str(&var("RECOVERY_TARGETS", "[]"))?;
    ensure!(
        !targets.is_empty()
            && targets
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == targets.len(),
        "Empty or duplicate expected targets"
    );
    ensure!(
        recovered.iter().all(|t| targets.contains(t)),
        "Recovery target is outside expected inventory"
    );
    targets
        .iter()
        .map(|t| {
            Identity::environment(
                t,
                &required(if recovered.contains(t) {
                    "RECOVERY_RUN_ID"
                } else {
                    "GITHUB_RUN_ID"
                })?,
            )
        })
        .collect()
}

pub fn run(args: &[String]) -> Result<()> {
    let (command, rest) = args.split_first().context("release requires plan|record|verify|manifests|stamp|publish|notes|version|fdroid|recovery|rollout")?;
    match command.as_str() {
        "plan" => {
            ensure!(rest.is_empty(), "plan takes environment inputs only");
            let mut p = plan(
                &required("SOURCE_TAG")?,
                &var("INPUT_PLATFORM", "all"),
                &var("RECOVERY_MODE", "build"),
                var("SHOULD_PUBLISH", "false") == "true",
                &var("RECOVERY_RUN_ID", ""),
                &var("GITHUB_EVENT_NAME", "workflow_dispatch"),
            )?;
            ensure!(
                p["version"] == common::version()?,
                "Tag version differs from collection version"
            );
            hours(&var("ROLLOUT_HOURS", "36"))?;
            let targets: Vec<String> = serde_json::from_value(p["expected"].clone())?;
            let component_plan = if p["mode"] == "publish-only" {
                let recovered = remote::recovery_plan()?;
                ensure!(
                    recovered
                        .targets
                        .iter()
                        .all(|target| targets.contains(&target.target)),
                    "Recovered plan exceeds the selected publication scope"
                );
                recovered
            } else {
                let (prior, package_hashes) = remote::prior_inventory(None)?;
                crate::components::plan::create(&targets, prior.as_ref(), &package_hashes)?
            };
            ensure!(
                component_plan.version == p["version"]
                    && component_plan.source_sha == required("SOURCE_SHA")?,
                "Component plan source mismatch"
            );
            let changed: Vec<_> = component_plan
                .targets
                .iter()
                .filter(|t| t.changed())
                .map(|t| t.target.clone())
                .collect();
            let recovered: Vec<String> = serde_json::from_value(p["recovery"].clone())?;
            let matrix: Vec<_> = changed
                .iter()
                .filter(|id| !recovered.contains(id))
                .map(|id| {
                    let mut value = target(id)?.matrix_entry()?;
                    let decision = component_plan
                        .targets
                        .iter()
                        .find(|t| t.target == *id)
                        .unwrap();
                    value["components"] = serde_json::to_value(&decision.build)?;
                    value["package"] = json!(decision.package);
                    Ok(value)
                })
                .collect::<Result<_>>()?;
            p["finalize"] = finalize_matrix(&matrix);
            // One job builds the browser client bundle that every daemon component ships.
            p["web"] = json!(matrix.iter().any(|entry: &Value| {
                let daemon = paseo_protocol::components::ComponentId::Daemon.as_str();
                entry["components"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|id| id == daemon))
            }));
            p["matrix"] = json!({"include":matrix});
            p["expected"] = json!(changed);
            p["recovery"] = json!(
                recovered
                    .into_iter()
                    .filter(|id| changed.contains(id))
                    .collect::<Vec<_>>()
            );
            p["noChanges"] = json!(component_plan.no_changes);
            common::write_json(
                &common::root()?.join("artifacts/component-plan.json"),
                &serde_json::to_value(component_plan)?,
            )?;
            for (k, v) in p.as_object().unwrap() {
                println!(
                    "{k}={}",
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                );
            }
            Ok(())
        }
        "record" => integrity::record(
            &directory()?,
            &Identity::environment(&required("TARGET_ID")?, &required("GITHUB_RUN_ID")?)?,
        ),
        "verify" => {
            if integrity::component_plan()?.no_changes {
                return Ok(());
            }
            for id in expected()? {
                integrity::verify(&directory()?, &id)?;
            }
            Ok(())
        }
        "manifests" => integrity::manifests(&directory()?),
        "stamp" => integrity::stamp(rest),
        "publish" => remote::publish(),
        "recovery" => remote::recovery(rest),
        "rollout" => remote::rollout(),
        "notes" => metadata::notes(rest),
        "version" => metadata::set_version(rest),
        "fdroid" => metadata::fdroid(rest),
        _ => bail!("Unknown release command {command}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_targets_and_recovery_are_exact() {
        let p = plan("v0.13.1", "all", "build", true, "", "push").unwrap();
        assert_eq!(p["expected"].as_array().unwrap().len(), 8);
        for id in ["linux-x64", "windows-arm64", "macos-x64"] {
            let p = plan("v0.13.1", id, "build", true, "123", "workflow_dispatch").unwrap();
            assert_eq!(p["expected"].as_array().unwrap().len(), 2);
            assert_eq!(p["recovery"].as_array().unwrap().len(), 1);
            assert!(plan("v0.13.1", id, "build", true, "", "workflow_dispatch").is_err());
        }
    }
    #[test]
    fn cross_targets_split_only_proven_recipes() {
        let p = plan("v0.13.4", "all", "build", false, "", "workflow_dispatch").unwrap();
        let entries = p["matrix"]["include"].as_array().unwrap();
        for entry in entries {
            let id = entry["id"].as_str().unwrap();
            assert_eq!(
                entry["triple"],
                common::BuildTarget::parse(id).unwrap().triple
            );
            match id {
                "windows-x64" | "macos-arm64" => {
                    assert_eq!(entry["runner"], "ubuntu-24.04");
                    assert_eq!(
                        entry["finalizer"],
                        if id == "windows-x64" {
                            "windows-latest"
                        } else {
                            "macos-15"
                        }
                    );
                }
                _ => assert!(entry["finalizer"].is_null()),
            }
        }
        let finalizers = p["finalize"]["include"].as_array().unwrap();
        assert_eq!(finalizers.len(), 2);
        for entry in finalizers {
            assert_eq!(entry["runner"], entry["finalizer"]);
        }
        let mut entry = target("windows-x64").unwrap().matrix_entry().unwrap();
        entry["components"] = json!(["app", "daemon"]);
        entry["package"] = json!(true);
        let finalize = finalize_matrix(&[entry]);
        assert_eq!(finalize["include"][0]["id"], "windows-x64");
        assert_eq!(
            finalize["include"][0]["components"],
            json!(["app", "daemon"])
        );
        assert_eq!(finalize["include"][0]["package"], true);
        assert_eq!(finalize_matrix(&[]), json!({"include":[]}));
    }
    #[test]
    fn publication_only_requires_prior_run() {
        assert!(
            plan(
                "v0.13.1",
                "all",
                "publish-only",
                true,
                "",
                "workflow_dispatch"
            )
            .is_err()
        );
        let p = plan(
            "desktop-v0.13.1",
            "all",
            "publish-only",
            true,
            "123",
            "workflow_dispatch",
        )
        .unwrap();
        assert_eq!(p["recovery"].as_array().unwrap().len(), 6);
        assert!(plan("v0.13.1", "all", "build", true, "123", "workflow_dispatch").is_err());
    }
    #[test]
    fn x64_android_publication_and_recovery_keep_exact_scope() {
        let scope = "linux-windows-x64-android";
        let targets = json!(["linux-x64", "windows-x64", "android-arm64"]);
        let build = plan("v0.13.4", scope, "build", true, "", "workflow_dispatch").unwrap();
        assert_eq!(build["expected"], targets);
        assert_eq!(build["recovery"], json!([]));
        assert_eq!(build["createRelease"], true);
        let recovery = plan(
            "v0.13.4",
            scope,
            "publish-only",
            true,
            "123",
            "workflow_dispatch",
        )
        .unwrap();
        assert_eq!(recovery["expected"], targets);
        assert_eq!(recovery["recovery"], targets);
        assert!(
            plan(
                "v0.13.4",
                scope,
                "publish-only",
                true,
                "",
                "workflow_dispatch"
            )
            .is_err()
        );
    }
    #[test]
    fn x64_android_and_apple_silicon_scope_adds_only_macos_arm64() {
        let scope = X64_ANDROID_MACOS_ARM64;
        let targets = json!(["linux-x64", "macos-arm64", "windows-x64", "android-arm64"]);
        let build = plan("v0.13.4", scope, "build", false, "", "workflow_dispatch").unwrap();
        let mut expected: Vec<_> = build["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_owned())
            .collect();
        expected.sort();
        let mut wanted: Vec<_> = targets
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_owned())
            .collect();
        wanted.sort();
        assert_eq!(expected, wanted);
        assert_eq!(build["recovery"], json!([]));
        assert_eq!(build["createRelease"], true);
        let recovery = plan(
            "v0.13.4",
            scope,
            "publish-only",
            true,
            "123",
            "workflow_dispatch",
        )
        .unwrap();
        assert_eq!(recovery["expected"], build["expected"]);
    }
    #[test]
    fn android_publication_only_accepts_arm64_apk() {
        let p = plan("v0.13.4", "android", "build", true, "", "workflow_dispatch").unwrap();
        assert_eq!(p["expected"], json!(["android-arm64"]));
        assert_eq!(p["matrix"]["include"][0]["format"], "apk");
        for removed in [
            "android-arm",
            "android-x64",
            "android-universal",
            "android-universal-aab",
        ] {
            assert!(target(removed).is_err());
            assert!(plan("v0.13.4", removed, "build", true, "", "workflow_dispatch").is_err());
        }
    }
    #[test]
    fn versions_and_rollout_are_strict() {
        for invalid in ["v1.2.3-rc.1", "v1.2.3+build", "v1.2", "v1.2.3-beta.x"] {
            assert!(tag_version(invalid).is_err());
        }
        for invalid in ["NaN", "inf", "-1", ""] {
            assert!(hours(invalid).is_err());
        }
        assert_eq!(channel(&tag_version("app-v0.13.1-beta.2").unwrap()), "beta");
    }
}
