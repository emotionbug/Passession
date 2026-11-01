use super::*;

#[test]
fn semver_requirements_preserve_ranges_and_prerelease_stable_core() {
    let manifest = json!({"id":"sample","requirements":{"paseo":"0.10.x || >=0.11.0 <0.12.0"}});
    assert!(manifest::check_version(&manifest, "0.11.0-beta.1").is_ok());
    assert!(manifest::check_version(&manifest, "0.10.9").is_ok());
    assert!(manifest::check_version(&manifest, "0.12.0").is_err());
    assert!(manifest::check_version(&json!({"id":"legacy"}), "0.8.0-beta.1").is_err());
    assert!(manifest::check_version(&json!({"id":"legacy"}), "0.7.9").is_ok());
}

#[test]
fn installation_identity_is_stricter_than_contribution_identity() {
    assert!(manifest::validate_id("sample-plugin").is_ok());
    assert!(manifest::validate_id("sample.provider").is_err());
    assert!(manifest::validate_id("sample_provider").is_err());
    assert!(manifest::validate_contribution_id("sample.provider_v2").is_ok());
    assert!(manifest::validate_contribution_id("../escape").is_err());
}

#[tokio::test]
async fn sample_manifest_rejects_unknown_requirements_and_traversing_assets() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paseo-plugin.json");
    tokio::fs::write(
        &path,
        br#"{"id":"sample","runtime":"native","server":{"executable":"target/release/sample","args":[]},"requirements":{"paseo":">=0.11.0"},"icon":"images/logo.png"}"#,
    )
    .await
    .unwrap();
    assert_eq!(
        manifest::read(directory.path()).await.unwrap()["id"],
        "sample"
    );
    tokio::fs::write(
        &path,
        br#"{"id":"sample","runtime":"native","server":{"executable":"target/release/sample","args":[]},"requirements":{"pasoe":">=0.11.0"}}"#,
    )
    .await
    .unwrap();
    assert!(manifest::read(directory.path()).await.is_err());
    tokio::fs::write(&path, br#"{"id":"sample","runtime":"native","server":{"executable":"sample","args":[]},"icon":"../logo.png"}"#)
        .await
        .unwrap();
    assert!(manifest::read(directory.path()).await.is_err());
}

#[tokio::test]
async fn provider_icons_cannot_execute_or_fetch_external_resources() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("icon.svg");
    tokio::fs::write(&path, br##"<svg><use href="#shape"/></svg>"##)
        .await
        .unwrap();
    assert!(manifest::icon(directory.path(), "icon.svg").await.is_ok());
    for svg in [
        "<svg onload='run()'></svg>",
        "<svg><script>run()</script></svg>",
        "<svg><use href='https://example.test/payload'/></svg>",
        "<svg><style>body{}</style></svg>",
    ] {
        tokio::fs::write(&path, svg).await.unwrap();
        assert!(manifest::icon(directory.path(), "icon.svg").await.is_err());
    }
}

#[tokio::test]
async fn hooks_reject_invalid_payloads_and_unknown_events() {
    let service = PluginService::new(Weak::new());
    assert!(
        service
            .before("agent.session_open", json!({"env":{}}))
            .await
            .is_err()
    );
    assert!(service.before("not.a.hook", json!({})).await.is_err());
    let valid = json!({"title":"t","source":{"kind":"directory","path":"/repo"}});
    assert_eq!(
        service
            .before("workspace.create", valid.clone())
            .await
            .unwrap(),
        valid
    );
    for invalid in [
        json!({"source":{"kind":"directory","path":"/repo"},"requestId":"r"}),
        json!({"source":{"kind":"directory","path":"/repo"},"unknown":1}),
        json!({"source":{"kind":"elsewhere"}}),
        json!({"title":"t"}),
        json!({"source":{"kind":"worktree"},"background":"yes"}),
    ] {
        assert!(service.before("workspace.create", invalid).await.is_err());
    }
}

#[tokio::test]
async fn native_plugins_require_contained_binary_and_preserve_argument_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paseo-plugin.json");
    tokio::fs::write(&path, br#"{"id":"legacy"}"#).await.unwrap();
    let error = manifest::read(directory.path()).await.unwrap_err().to_string();
    assert!(error.contains("native Rust plugin release"));
    assert!(error.contains("settings and installation records are preserved"));
    let mut value = json!({"id":"sample","runtime":"native","server":{"executable":"plugin-bin","args":["two words",";not a shell",""]}});
    tokio::fs::write(&path, serde_json::to_vec(&value).unwrap()).await.unwrap();
    // Metadata may be inspected before Cargo has produced the executable.
    assert!(manifest::read(directory.path()).await.is_ok());
    assert!(manifest::native_entry(directory.path()).await.is_err());
    let binary = directory.path().join("plugin-bin");
    tokio::fs::copy(std::env::current_exe().unwrap(), &binary).await.unwrap();
    let entry = manifest::native_entry(directory.path()).await.unwrap();
    assert_eq!(entry.args, ["two words", ";not a shell", ""]);
    assert_eq!(entry.executable, tokio::fs::canonicalize(&binary).await.unwrap());
    tokio::fs::write(&binary, b"#!/usr/bin/env node\n").await.unwrap();
    assert!(manifest::native_entry(directory.path()).await.unwrap_err().to_string().contains("not a script"));
    for executable in ["../escape", "/absolute", "C:\\escape", "dir/../escape", "dir//file", "./file"] {
        value["server"]["executable"] = json!(executable);
        tokio::fs::write(&path, serde_json::to_vec(&value).unwrap()).await.unwrap();
        assert!(manifest::read(directory.path()).await.is_err(), "{executable}");
    }
}

#[tokio::test]
async fn native_manifest_rejects_invalid_argument_vectors() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("paseo-plugin.json");
    for args in [Value::Null, json!("--flag value"), json!([1]), json!(["bad\u{0}argument"])] {
        let value = json!({"id":"sample","runtime":"native","server":{"executable":"plugin-bin","args":args}});
        tokio::fs::write(&path, serde_json::to_vec(&value).unwrap()).await.unwrap();
        assert!(manifest::read(directory.path()).await.is_err());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn native_executable_symlinks_cannot_escape_installation() {
    let directory = tempfile::tempdir().unwrap();
    tokio::fs::write(directory.path().join("paseo-plugin.json"), br#"{"id":"sample","runtime":"native","server":{"executable":"plugin-bin","args":[]}}"#).await.unwrap();
    std::os::unix::fs::symlink(std::env::current_exe().unwrap(), directory.path().join("plugin-bin")).unwrap();
    assert!(manifest::native_entry(directory.path()).await.unwrap_err().to_string().contains("escapes"));
}

#[test]
fn log_tail_bounds_entries_and_utf8_without_losing_sequence() {
    let service = PluginService::new(Weak::new());
    for _ in 0..600 {
        service.append_log("muse-provider", "stdout", "message");
    }
    let logs = service.get_logs("muse-provider").unwrap();
    assert_eq!(logs.len(), 500);
    assert_eq!(logs[0]["sequence"], 101);
    service.append_log("muse-provider", "stderr", &"한".repeat(6000));
    let logs = service.get_logs("muse-provider").unwrap();
    let message = logs.last().unwrap()["message"].as_str().unwrap();
    assert!(message.len() <= 16384);
    assert!(message.chars().all(|character| character == '한'));
}
