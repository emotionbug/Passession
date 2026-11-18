use super::{storage, transactions};
use serde_json::json;

#[test]
fn malformed_persisted_configuration_is_not_replaced() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.json");
    std::fs::write(&path, "{\"daemon\":").unwrap();
    assert!(storage::read(home.path(), true, true).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "{\"daemon\":");
}

#[test]
fn unknown_persisted_settings_do_not_block_loading_or_change_current_settings() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.json");
    let plugins =
        json!({"omp-kiro":{"source":"directory","path":"/plugins/omp-kiro","enabled":false}});
    let original = json!({
        "version":1,
        "daemon":{"listen":"127.0.0.1:6767","hostnames":["current.example"],"allowedHosts":["old.example"],"browserTools":null},
        "features":{"dictation":null,"voiceMode":true,"webUi":{"enabled":true}},
        "providers":{"old":{"apiKey":"unused"}},
        "plugins":plugins,
        "agents":{"providers":{"claude":{"enabled":false}}}
    });
    std::fs::write(&path, original.to_string()).unwrap();
    let loaded = storage::read(home.path(), false, false).unwrap();
    assert_eq!(
        loaded,
        json!({
            "version":1,
            "daemon":{"listen":"127.0.0.1:6767","hostnames":["current.example"]},
            "features":{},
            "plugins":plugins,
            "agents":{"providers":{"claude":{"enabled":false}}}
        })
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), original.to_string());
    let resolved = super::resolve::resolve(
        home.path(),
        home.path(),
        &loaded,
        &std::collections::BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(resolved["providerOverrides"], loaded["agents"]["providers"]);
    assert_eq!(resolved["plugins"], plugins);
}

#[test]
fn profile_replacement_preserves_unknown_metadata_and_feature_nulls() {
    let home = tempfile::tempdir().unwrap();
    let original = json!({"version":1,"daemon":{"agentProfiles":[{"id":"existing","name":"Existing","provider":"claude"}]}});
    storage::save(home.path(), &original).unwrap();
    let patch = transactions::supported_patch(json!({"agentProfiles":[{"id":"custom","name":"Custom","provider":"claude","cliDefault":false,"featureValues":{"enabled":false,"clear":null},"futureMetadata":{"id":17}}],"terminalProfiles":[]}),false).unwrap();
    let next = transactions::persist_patch(&original, &patch, &[]).unwrap();
    storage::save(home.path(), &next).unwrap();
    let stored = storage::read(home.path(), false, false).unwrap();
    assert_eq!(stored["daemon"]["agentProfiles"], patch["agentProfiles"]);
    assert_eq!(stored["daemon"]["terminalProfiles"], json!([]));
    assert!(stored["daemon"]["agentProfiles"][0].get("modeId").is_none());
}

#[test]
fn provider_removal_prunes_metadata_without_touching_profiles() {
    let original = json!({"agents":{"providers":{"custom":{"extends":"claude","command":["custom-agent"]}},"metadataGeneration":{"providers":[{"provider":"custom"},{"provider":"claude"}]}},"daemon":{"agentProfiles":[{"id":"p","name":"P","provider":"custom"}]}});
    let next = transactions::persist_patch(&original, &json!({}), &["custom".to_owned()]).unwrap();
    assert!(next["agents"].get("providers").is_none());
    assert_eq!(
        next["agents"]["metadataGeneration"]["providers"],
        json!([{"provider":"claude"}])
    );
    assert_eq!(
        next["daemon"]["agentProfiles"],
        original["daemon"]["agentProfiles"]
    );
}

#[test]
fn plugin_opt_out_survives_config_updates_and_reload() {
    let home = tempfile::tempdir().unwrap();
    let environment = std::collections::BTreeMap::new();
    let initial =
        json!({"plugins":{"held":{"source":"directory","path":"/plugins/held","enabled":false}}});
    let resolve = |persisted: &serde_json::Value| {
        super::resolve::resolve(home.path(), home.path(), persisted, &environment).unwrap()
    };
    assert_eq!(resolve(&initial)["pluginsEnabled"], true);
    let disabled =
        transactions::persist_patch(&initial, &json!({"pluginsEnabled":false}), &[]).unwrap();
    let updated = transactions::persist_patch(
        &disabled,
        &json!({"appendSystemPrompt":"Keep plugin opt-out"}),
        &[],
    )
    .unwrap();
    storage::save(home.path(), &updated).unwrap();
    let reloaded = storage::read(home.path(), false, false).unwrap();
    let resolved = resolve(&reloaded);
    assert_eq!(resolved["pluginsEnabled"], false);
    assert_eq!(super::resolve::mutable(&resolved)["pluginsEnabled"], false);
    let enabled =
        transactions::persist_patch(&reloaded, &json!({"pluginsEnabled":true}), &[]).unwrap();
    let resolved = resolve(&enabled);
    assert_eq!(resolved["pluginsEnabled"], true);
    assert_eq!(resolved["plugins"]["held"]["enabled"], false);
}

#[test]
fn reload_diff_distinguishes_missing_and_explicit_null() {
    let mut paths = Vec::new();
    transactions::diff(
        Some(&json!({"option":null})),
        Some(&json!({})),
        "",
        &mut paths,
    );
    assert_eq!(paths, vec!["option"]);
}

#[cfg(unix)]
#[test]
fn atomic_configuration_write_preserves_symlink_and_private_mode() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let actual = root.path().join("actual.json");
    std::fs::write(&actual, "{}").unwrap();
    symlink(&actual, home.join("config.json")).unwrap();
    storage::save(&home, &json!({"version":1})).unwrap();
    assert!(
        std::fs::symlink_metadata(home.join("config.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        storage::read(&home, false, false).unwrap(),
        json!({"version":1})
    );
    assert_eq!(
        std::fs::metadata(actual).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn launch_overrides_survive_reload_without_erasing_hostname_sources() {
    let home = tempfile::tempdir().unwrap();
    let environment = std::collections::BTreeMap::from([
        ("PASEO_HOSTNAMES".to_owned(), "env.example".to_owned()),
        ("PASEO_RELAY_USE_TLS".to_owned(), "false".to_owned()),
    ]);
    let cli = json!({"relayEnabled":false,"relayUseTls":true,"mcpEnabled":false,"hostnames":["cli.example"]});
    let persisted = json!({"daemon":{"hostnames":["disk.example"],"relay":{"enabled":true}}});
    let initial = super::resolve::resolve_with_options(
        home.path(),
        home.path(),
        &persisted,
        &environment,
        None,
        &cli,
    )
    .unwrap();
    assert_eq!(
        initial["hostnames"],
        json!(["disk.example", "env.example", "cli.example"])
    );
    assert_eq!(initial["relayEnabled"], false);
    assert_eq!(initial["relayEnabledMutable"], false);
    assert_eq!(initial["relayUseTls"], true);
    assert_eq!(initial["relayPublicUseTls"], true);
    assert_eq!(initial["mcpEnabled"], false);
    let changed = json!({"daemon":{"hostnames":["changed.example"]}});
    let reloaded = super::resolve::resolve_with_options(
        home.path(),
        home.path(),
        &changed,
        &environment,
        Some(false),
        &cli,
    )
    .unwrap();
    assert_eq!(
        reloaded["hostnames"],
        json!(["changed.example", "env.example", "cli.example"])
    );
    assert_eq!(reloaded["relayEnabled"], false);
    assert_eq!(reloaded["mcpEnabled"], false);
}

#[test]
fn tray_setting_resolves_persisted_and_foreground_opt_out() {
    let home = tempfile::tempdir().unwrap();
    let resolve = |persisted, cli| {
        super::resolve::resolve_with_options(
            home.path(),
            home.path(),
            &persisted,
            &Default::default(),
            None,
            &cli,
        )
        .unwrap()
    };
    assert_eq!(resolve(json!({}), json!({}))["tray"], true);
    let persisted = super::schema::normalize_persisted(json!({"daemon":{"tray":false}})).unwrap();
    assert_eq!(resolve(persisted, json!({}))["tray"], false);
    let overridden = resolve(json!({"daemon":{"tray":true}}), json!({"tray":false}));
    assert_eq!(overridden["tray"], false);
    assert_eq!(
        overridden["configReload"]["overrideControlledPaths"],
        json!(["daemon.tray"])
    );
    assert!(super::schema::normalize_persisted(json!({"daemon":{"tray":"false"}})).is_err());
}

#[test]
fn json_failure_is_not_accepted_as_a_different_document_format() {
    assert!(storage::parse_document(std::path::Path::new("config.json"), "version: 1").is_err());
    assert_eq!(
        storage::parse_document(std::path::Path::new("config.yaml"), "version: 1").unwrap(),
        json!({"version":1})
    );
    assert_eq!(
        storage::parse_document(
            std::path::Path::new("config.json"),
            "\u{feff}{\"version\":1}"
        )
        .unwrap(),
        json!({"version":1})
    );
}

#[test]
fn public_patch_cannot_bypass_the_skills_transaction() {
    assert_eq!(
        transactions::supported_patch(json!({"skills":null}), false).unwrap(),
        json!({})
    );
    assert!(transactions::supported_patch(json!({"skills":null}), true).is_err());
}

#[test]
fn editable_configuration_paths_stop_at_dynamic_boundaries() {
    for path in [
        "daemon.listen",
        "agents.providers",
        "agents.skills.selection",
        "agents.idleRuntimeTimeoutMs",
        "daemon.agentProfiles",
        "worktrees.servicePorts.range",
    ] {
        assert!(super::validate_persisted_path(path).is_ok(), "{path}");
    }
    for path in [
        "daemon..listen",
        "daemon.__proto__",
        "agents.providers.claude",
        "daemon.agentProfiles.0",
        "agents.skills.selection.mode",
        "daemon.browserTools",
        "features.webUi",
        "providers",
        "features.dictation",
        "features.voiceMode",
    ] {
        assert!(super::validate_persisted_path(path).is_err(), "{path}");
    }
}

#[tokio::test]
async fn offline_edits_cannot_change_authentication_through_daemon_replacement() {
    let home = tempfile::tempdir().unwrap();
    assert!(
        super::edit_persisted(home.path(), "daemon.auth.password", Some(json!("secret")))
            .await
            .is_err()
    );
    assert!(!home.path().join("config.json").exists());
    let password = format!("$2b$12${}", "a".repeat(53));
    storage::save(
        home.path(),
        &json!({"daemon":{"listen":"127.0.0.1:6767","auth":{"password":password}}}),
    )
    .unwrap();
    assert!(
        super::edit_persisted(
            home.path(),
            "daemon",
            Some(json!({"listen":"127.0.0.1:7000"}))
        )
        .await
        .is_err()
    );
    assert!(
        super::edit_persisted(home.path(), "daemon", None)
            .await
            .is_err()
    );
    super::edit_persisted(home.path(), "daemon.listen", Some(json!("127.0.0.1:7000")))
        .await
        .unwrap();
    let persisted = storage::read(home.path(), false, false).unwrap();
    assert_eq!(persisted["daemon"]["auth"]["password"], password);
    assert_eq!(persisted["daemon"]["listen"], "127.0.0.1:7000");
}
