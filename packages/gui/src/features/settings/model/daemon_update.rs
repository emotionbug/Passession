//! Durable daemon update rules from `daemon_update.dart` and
//! `core/daemon_maintenance.dart`.

use serde_json::Value;

use crate::features::settings::json::{get, str_of};

pub const DURABLE_FEATURE: &str = "durableDaemonUpdates";
pub const COMPONENT_IDS: [&str; 4] = ["app", "daemon", "tailnet", "update-helper"];
pub const APP_COMPONENT_IDS: [&str; 3] = ["app", "tailnet", "update-helper"];

pub fn supports_durable(server_info: &Value) -> bool {
    if server_info["desktopManaged"] == true {
        supports_app(server_info)
    } else {
        crate::features::settings::json::feature(server_info, DURABLE_FEATURE)
    }
}

pub fn supports_app(server_info: &Value) -> bool {
    crate::features::settings::json::feature(server_info, "desktopComponentUpdates")
}

pub fn supports_check(server_info: &Value) -> bool {
    if server_info["desktopManaged"] == true {
        supports_app(server_info)
    } else {
        crate::features::settings::json::feature(server_info, "daemonUpdateCheck")
    }
}

pub fn target_kind(server_info: &Value) -> &'static str {
    if server_info["desktopManaged"] == true {
        "app"
    } else {
        "daemon"
    }
}

/// A desktop capability never permits a standalone fallback or a stale check.
pub fn can_update_host(server_info: &Value, check: &Value) -> bool {
    can_update_checked(check)
        && if target_kind(server_info) == "app" {
            supports_app(server_info)
                && check["targetKind"] == "app"
                && check["owner"]["kind"] == "desktopApp"
                && selection_valid(check, &changed_components(check))
        } else {
            check["targetKind"].is_null() || check["targetKind"] == "daemon"
        }
}

pub fn can_update_checked(check: &Value) -> bool {
    check["supported"] == true
        && check["updateAvailable"] == true
        && can_pin_release(&check["latest"])
        && check["checkError"].is_null()
}

pub fn can_pin_release(release: &Value) -> bool {
    release["version"].as_str().is_some_and(|v| !v.is_empty())
        && release["manifestSha512"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
}

/// The selected set is always explicit in the persisted approval.
pub fn changed_components(check: &Value) -> Vec<String> {
    let mut selected: Vec<String> = check["availableComponents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|component| component["id"].as_str().map(String::from))
        .collect();
    selected.sort_unstable();
    selected
}

/// Signed cohorts can expose multiple primary images in one archive.
pub fn download_bytes(check: &Value, selected: &[String]) -> u64 {
    let Some(available) = check["availableComponents"].as_array() else {
        return 0;
    };
    let selected_entry = |entry: &Value| selected.iter().any(|id| entry["id"] == id.as_str());
    available
        .iter()
        .enumerate()
        .filter(|(index, entry)| {
            selected_entry(entry)
                && !available[..*index].iter().any(|previous| {
                    selected_entry(previous)
                        && previous["artifactSha256"] == entry["artifactSha256"]
                })
        })
        .filter_map(|(_, entry)| entry["downloadSize"].as_u64())
        .fold(0_u64, u64::saturating_add)
}

pub fn restart_group(selected: &[String]) -> &'static str {
    if selected
        .iter()
        .any(|id| matches!(id.as_str(), "app" | "tailnet"))
    {
        "gui"
    } else if selected.iter().any(|id| id == "daemon") {
        "daemon"
    } else {
        "none"
    }
}

/// Validate the retained and selected ABI graph together. The owner repeats
/// this check against its verified inventory before admitting the request.
pub fn selection_valid(check: &Value, selected: &[String]) -> bool {
    let Some(current) = check["components"].as_object() else {
        return false;
    };
    let Some(available) = check["availableComponents"].as_array() else {
        return false;
    };
    let Some(requirements) = check["componentRequirements"].as_object() else {
        return false;
    };
    let Some(groups) = check["atomicGroups"].as_array() else {
        return false;
    };
    if current.len() != COMPONENT_IDS.len()
        || requirements.len() != COMPONENT_IDS.len()
        || !COMPONENT_IDS
            .iter()
            .all(|id| current.get(*id).is_some_and(valid_identity))
        || selected.iter().enumerate().any(|(index, id)| {
            !COMPONENT_IDS.contains(&id.as_str())
                || selected[..index].contains(id)
                || available
                    .iter()
                    .filter(|entry| entry["id"] == id.as_str())
                    .count()
                    != 1
        })
    {
        return false;
    }
    for group in groups {
        let Some(group) = group.as_array() else {
            return false;
        };
        if group
            .iter()
            .any(|id| selected.iter().any(|selected| id == selected.as_str()))
        {
            for id in group {
                if available.iter().any(|entry| entry["id"] == *id)
                    && !selected.iter().any(|selected| id == selected.as_str())
                {
                    return false;
                }
            }
        }
    }
    let chosen = |id: &str| {
        selected
            .iter()
            .any(|selected| selected == id)
            .then(|| available.iter().find(|entry| entry["id"] == id))
            .flatten()
    };
    COMPONENT_IDS.iter().all(|id| {
        let entry = chosen(id);
        let identity = entry.map(|entry| &entry["target"]).unwrap_or(&current[*id]);
        let requires = entry
            .map(|entry| &entry["requires"])
            .or_else(|| requirements.get(*id));
        valid_identity(identity)
            && requires.and_then(Value::as_object).is_some_and(|requires| {
                requires.iter().all(|(dependency, abi)| {
                    COMPONENT_IDS.contains(&dependency.as_str())
                        && abi.as_u64().is_some_and(|abi| {
                            let actual = chosen(dependency)
                                .map(|entry| &entry["target"])
                                .unwrap_or(&current[dependency.as_str()]);
                            actual["abiVersion"].as_u64() == Some(abi)
                        })
                })
            })
    })
}

fn valid_identity(identity: &Value) -> bool {
    let hex = |key: &str, len| {
        identity[key].as_str().is_some_and(|value| {
            value.len() == len && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    };
    identity["version"]
        .as_str()
        .is_some_and(|version| !version.is_empty())
        && hex("sourceSha", 40)
        && hex("inputHash", 64)
        && identity["abiVersion"]
            .as_u64()
            .is_some_and(|abi| abi > 0 && abi <= u32::MAX as u64)
}

/// The independent app helper verifies the complete app-side selection before
/// projecting a committed journal. No daemon component belongs to that result.
pub fn remote_app_commit_verified(operation: &Value) -> bool {
    operation["outcome"] == "committed"
        && operation["candidate"]["version"] == operation["expectedRelease"]["version"]
        && operation["observedComponents"] == operation["targetComponents"]
        && operation["targetComponents"]
            .as_object()
            .is_some_and(|components| {
                components.len() == APP_COMPONENT_IDS.len()
                    && APP_COMPONENT_IDS
                        .iter()
                        .all(|id| components.get(*id).is_some_and(valid_identity))
            })
}

/// The standalone executor records the authenticated replacement worker and
/// its running image digest before it commits. That evidence also applies to
/// an operation restored after the host has moved on to a newer runtime.
pub fn committed_daemon_matches(operation: &Value, server_id: &str) -> bool {
    let candidate = get(operation, "candidate");
    let worker = get(operation, "worker");
    let version = str_of(candidate, "version");
    let digest = str_of(candidate, "executableSha256");
    outcome(operation) == Outcome::Committed
        && str_of(operation, "serverId") == server_id
        && str_of(worker, "serverId") == server_id
        && !version.is_empty()
        && str_of(worker, "version") == version
        && !digest.is_empty()
        && str_of(worker, "executableSha256") == digest
}

/// For desktop updates compare the complete selection with the live owner.
pub fn committed_app_components_match(operation: &Value, actual: &Value, server_id: &str) -> bool {
    let Some(expected) = operation["targetComponents"].as_object() else {
        return false;
    };
    outcome(operation) == Outcome::Committed
        && operation["targetKind"] == "app"
        && operation["outcome"] == "committed"
        && str_of(operation, "serverId") == server_id
        && str_of(actual, "serverId") == server_id
        && expected.len() == COMPONENT_IDS.len()
        && COMPONENT_IDS
            .iter()
            .all(|id| expected.get(*id).is_some_and(valid_identity))
        && actual["targetKind"] == "app"
        && actual["owner"]["kind"] == "desktopApp"
        && operation["observedComponents"] == operation["targetComponents"]
        && actual["components"] == operation["targetComponents"]
}

/// Confirmation stays bound to the exact pin and component selection flushed
/// before submission, not the collection version alone.
pub fn committed_release_matches(operation: &Value, request: Option<&Value>) -> bool {
    if operation["targetKind"] == "app" {
        let Some(request) = request else { return false };
        return request["targetKind"] == "app"
            && operation["operationId"] == request["operationId"]
            && request["expectedRelease"].is_object()
            && operation["expectedRelease"] == request["expectedRelease"]
            && request["selectedComponents"].is_array()
            && operation["selectedComponents"] == request["selectedComponents"];
    }
    let Some(request) = request else { return true };
    let expected = str_of(get(request, "expectedRelease"), "version");
    (expected.is_empty() || str_of(get(operation, "candidate"), "version") == expected)
        && request["targetKind"] != "app"
}

/// The operation's own result; a session resume never changes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    Running,
    Committed,
    RolledBack,
    Failed,
}

pub fn outcome(operation: &Value) -> Outcome {
    if operation.get("terminal").and_then(Value::as_bool) != Some(true) {
        return Outcome::Running;
    }
    match str_of(operation, "phase") {
        "committed" => Outcome::Committed,
        "rolled_back" | "rolledBack" => Outcome::RolledBack,
        _ => Outcome::Failed,
    }
}

/// `continuation.state` when a session resume was requested and is not accepted.
pub fn unconfirmed_continuation(operation: &Value) -> Option<String> {
    let continuation = operation.get("continuation")?;
    if !continuation.is_object() {
        return None;
    }
    let state = match str_of(continuation, "state") {
        "" => "unknown",
        other => other,
    };
    if state == "accepted" {
        None
    } else {
        Some(state.to_string())
    }
}

/// Whether to keep the operation id to inspect it again (never resubmits).
pub fn retain_operation(operation: &Value) -> bool {
    if operation.get("terminal").and_then(Value::as_bool) != Some(true) {
        return true;
    }
    if operation["recoveryRequired"] == true {
        return true;
    }
    if outcome(operation) != Outcome::Committed {
        return false;
    }
    matches!(
        unconfirmed_continuation(operation).as_deref(),
        Some("pending") | Some("unknown")
    )
}

/// Locale key suffix under `settings.host.daemon.update`.
pub fn phase_key(phase: &str) -> &'static str {
    match phase {
        "admitted" => "phaseStarting",
        "preparing" | "preparation" => "phaseDownloading",
        "staged" => "phaseInstalling",
        "draining" => "phaseDraining",
        "waitingForParent" => "phaseWaitingForParent",
        "waitingForAuthorization" => "phaseWaitingForAuthorization",
        "installing" => "phaseInstalling",
        "activating" => "phaseActivating",
        "starting" => "phaseAppStarting",
        "verifying" | "checking" => "phaseVerifying",
        "rolling_back" | "rollingBack" | "rolled_back" | "rolledBack" => "phaseRollingBack",
        "committed" => "phaseComplete",
        _ => "phaseInstalling",
    }
}

pub fn new_operation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Refuse a different operation or a journal belonging to another host.
pub fn operation_of(
    response: &Value,
    operation_id: &str,
    server_id: &str,
) -> Result<Value, String> {
    let operation = get(response, "operation").clone();
    let reported = str_of(&operation, "serverId");
    if str_of(&operation, "operationId") != operation_id
        || (!reported.is_empty() && reported != server_id)
        || (operation["targetKind"] == "app" && reported != server_id)
    {
        return Err(crate::features::settings::i18n::t(
            "settingsNative.otherOperation",
        ));
    }
    Ok(operation)
}

/// `"v1.2.3"` → `"1.2.3"`; empty → `None`.
pub fn normalize_version(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(
        trimmed
            .strip_prefix(['v', 'V'])
            .unwrap_or(trimmed)
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn committed_daemon_requires_the_recorded_worker_image_version_and_host() {
        let mut operation = json!({"terminal":true,"phase":"committed","serverId":"one",
            "candidate":{"version":"2.0.0","executableSha256":"a".repeat(64)},
            "worker":{"serverId":"one","version":"2.0.0","executableSha256":"a".repeat(64)}});
        assert!(committed_daemon_matches(&operation, "one"));
        assert!(!committed_daemon_matches(&operation, "two"));
        for (key, value) in [
            ("serverId", "two"),
            ("version", "1.0.0"),
            ("executableSha256", ""),
        ] {
            let mut changed = operation.clone();
            changed["worker"][key] = json!(value);
            assert!(!committed_daemon_matches(&changed, "one"), "{key}");
        }
        operation["phase"] = json!("rolled_back");
        assert!(!committed_daemon_matches(&operation, "one"));
    }

    #[test]
    fn release_check_never_enables_unsupported_unverified_or_current_release() {
        let checked = json!({"supported":true,"updateAvailable":true,"checkError":null,
            "latest":{"version":"2.0.0","manifestSha512":"verified-manifest"}});
        assert!(can_update_checked(&checked));
        for (key, value) in [
            ("supported", json!(false)),
            ("updateAvailable", json!(false)),
            ("latest", Value::Null),
            ("checkError", json!("offline")),
        ] {
            let mut changed = checked.clone();
            changed[key] = value;
            assert!(!can_update_checked(&changed), "{key}");
        }
    }

    #[test]
    fn committed_with_pending_resume_is_retained_but_rollback_is_not() {
        let committed =
            json!({"terminal": true, "phase": "committed", "continuation": {"state": "pending"}});
        assert_eq!(outcome(&committed), Outcome::Committed);
        assert!(retain_operation(&committed));
        assert!(!retain_operation(
            &json!({"terminal": true, "phase": "committed"})
        ));
        assert!(!retain_operation(
            &json!({"terminal": true, "phase": "rolled_back"})
        ));
        assert!(retain_operation(
            &json!({"terminal": false, "phase": "draining"})
        ));
        assert_eq!(
            unconfirmed_continuation(&json!({"continuation": {}})).as_deref(),
            Some("unknown")
        );
        assert_eq!(
            unconfirmed_continuation(&json!({"continuation": {"state": "accepted"}})),
            None
        );
    }

    fn identity(index: usize) -> Value {
        json!({"version":format!("1.0.{index}"),"sourceSha":format!("{index:040x}"),
            "inputHash":format!("{index:064x}"),"abiVersion":if index == 3 {2} else {1}})
    }

    fn desktop_check(changed: &[&str]) -> Value {
        let mut check = json!({"serverId":"one","supported":true,
            "updateAvailable":!changed.is_empty(),"targetKind":"app",
            "owner":{"kind":"desktopApp"},"latest":{"version":"2.0.0","manifestSha512":"A".repeat(88)},
            "components":{},"componentRequirements":{},"availableComponents":[],"atomicGroups":[]});
        for (index, id) in COMPONENT_IDS.iter().enumerate() {
            check["components"][*id] = identity(index);
            check["componentRequirements"][*id] = json!({});
            if changed.contains(id) {
                let mut target = identity(index + 4);
                target["abiVersion"] = check["components"][*id]["abiVersion"].clone();
                let change = json!({"id":id,"current":check["components"][*id],
                    "target":target,"downloadSize":1024,"requires":{}});
                check["availableComponents"]
                    .as_array_mut()
                    .unwrap()
                    .push(change);
            }
        }
        check
    }

    #[test]
    fn desktop_updates_require_component_owner_not_old_installer_capability() {
        let host = json!({"desktopManaged":true,"features":{"desktopComponentUpdates":true}});
        let checked = desktop_check(&["daemon"]);
        assert!(supports_durable(&host));
        assert!(supports_check(&host));
        assert!(can_update_host(&host, &checked));
        for changed in [
            json!({"desktopManaged":true,"features":{"desktopAppUpdates":true,"daemonUpdateCheck":true}}),
            json!({"desktopManaged":true,"features":{"durableDaemonUpdates":true}}),
            json!({"desktopManaged":false,"features":{"durableDaemonUpdates":true}}),
        ] {
            assert!(!can_update_host(&changed, &checked));
            if changed["desktopManaged"] == true {
                assert!(!supports_durable(&changed));
                assert!(!supports_check(&changed));
            }
        }
        for (key, value) in [
            ("owner", Value::Null),
            ("targetKind", json!("daemon")),
            ("supported", json!(false)),
        ] {
            let mut changed = checked.clone();
            changed[key] = value;
            assert!(!can_update_host(&host, &changed), "{key}");
        }
    }

    #[test]
    fn component_plan_selects_only_changes_and_classifies_restarts() {
        for (changes, group) in [
            (vec!["update-helper"], "none"),
            (vec!["daemon"], "daemon"),
            (vec!["app"], "gui"),
            (vec!["tailnet"], "gui"),
            (vec!["daemon", "update-helper"], "daemon"),
            (vec![], "none"),
        ] {
            let checked = desktop_check(&changes);
            let selected = changed_components(&checked);
            assert_eq!(selected, changes);
            assert_eq!(restart_group(&selected), group);
            assert!(selection_valid(&checked, &selected));
            assert_eq!(can_update_checked(&checked), !selected.is_empty());
        }
    }

    #[test]
    fn incompatible_partial_selection_rejects_dependencies_and_atomic_groups() {
        let mut checked = desktop_check(&["app", "tailnet"]);
        checked["componentRequirements"]["app"] = json!({"tailnet":1});
        checked["availableComponents"][0]["requires"] = json!({"tailnet":2});
        checked["availableComponents"][1]["target"]["abiVersion"] = json!(2);
        let selected = changed_components(&checked);
        assert!(selection_valid(&checked, &selected));
        assert!(!selection_valid(&checked, &["app".into()]));
        assert!(!selection_valid(&checked, &["tailnet".into()]));
        assert!(!selection_valid(&checked, &["daemon".into()]));
        assert!(!selection_valid(&checked, &["app".into(), "app".into()]));
        checked = desktop_check(&["app", "tailnet"]);
        checked["atomicGroups"] = json!([["app", "tailnet"]]);
        assert!(!selection_valid(&checked, &["app".into()]));
        assert!(selection_valid(&checked, &selected));
        checked["availableComponents"][1]["target"] = checked["components"]["tailnet"].clone();
        assert_eq!(changed_components(&checked), ["app", "tailnet"]);
        assert!(!selection_valid(&checked, &["app".into()]));
        assert!(selection_valid(&checked, &selected));
        checked["availableComponents"][0]["artifactSha256"] = json!("a".repeat(64));
        checked["availableComponents"][1]["artifactSha256"] = json!("a".repeat(64));
        assert_eq!(download_bytes(&checked, &selected), 1024);
        assert_eq!(download_bytes(&checked, &["tailnet".into()]), 1024);
        assert_eq!(download_bytes(&checked, &[]), 0);
        checked["availableComponents"][1]["artifactSha256"] = json!("b".repeat(64));
        assert_eq!(download_bytes(&checked, &selected), 2048);
    }

    #[test]
    fn committed_components_require_actual_complete_identities_not_equal_versions() {
        for (changed, group) in [
            ("update-helper", "none"),
            ("daemon", "daemon"),
            ("app", "gui"),
            ("tailnet", "gui"),
        ] {
            let mut actual = desktop_check(&[changed]);
            actual["components"][changed] = actual["availableComponents"][0]["target"].clone();
            let operation = json!({"operationId":"op","serverId":"one","targetKind":"app",
                "terminal":true,"phase":"committed","outcome":"committed","restartGroup":group,
                "targetComponents":actual["components"],"observedComponents":actual["components"]});
            assert!(committed_app_components_match(&operation, &actual, "one"));
            assert_ne!(
                actual["components"]["app"]["version"],
                actual["components"]["daemon"]["version"]
            );
            for id in COMPONENT_IDS {
                for field in ["version", "sourceSha", "inputHash", "abiVersion"] {
                    let mut stale = actual.clone();
                    stale["components"][id][field] = Value::Null;
                    assert!(
                        !committed_app_components_match(&operation, &stale, "one"),
                        "{changed}/{id}/{field}"
                    );
                }
            }
            assert!(!committed_app_components_match(
                &operation, &actual, "other"
            ));
            for phase in [
                "waitingForParent",
                "waitingForAuthorization",
                "installing",
                "starting",
                "checking",
            ] {
                let mut pending = operation.clone();
                pending["phase"] = json!(phase);
                assert!(!committed_app_components_match(&pending, &actual, "one"));
            }
            let mut unobserved = operation.clone();
            unobserved["observedComponents"] = Value::Null;
            assert!(!committed_app_components_match(&unobserved, &actual, "one"));
            unobserved["phase"] = json!("rolledBack");
            unobserved["recoveryRequired"] = json!(true);
            assert!(retain_operation(&unobserved));
        }
        assert_eq!(
            phase_key("waitingForAuthorization"),
            "phaseWaitingForAuthorization"
        );
    }

    #[test]
    fn component_confirmation_binds_restored_full_pin_selection_and_host() {
        let request = json!({"operationId":"op","targetKind":"app","selectedComponents":["daemon"],
            "expectedRelease":{"version":"2.0.0","manifestSha512":"A".repeat(88)}});
        let mut operation = request.clone();
        operation["serverId"] = json!("one");
        assert!(committed_release_matches(&operation, Some(&request)));
        assert!(!committed_release_matches(&operation, None));
        for (pointer, value) in [
            ("/expectedRelease/version", json!("3.0.0")),
            ("/expectedRelease/manifestSha512", json!("B".repeat(88))),
            ("/selectedComponents", json!(["app"])),
            ("/operationId", json!("other")),
            ("/targetKind", json!("daemon")),
        ] {
            let mut changed = operation.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            assert!(
                !committed_release_matches(&changed, Some(&request)),
                "{pointer}"
            );
        }
        let response = json!({"operation":operation});
        assert!(operation_of(&response, "op", "one").is_ok());
        assert!(operation_of(&response, "op", "two").is_err());
        assert!(operation_of(&response, "other", "one").is_err());
    }
}
