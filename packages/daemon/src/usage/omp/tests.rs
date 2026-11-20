use super::*;
use crate::providers::api::ProviderSpec;
use crate::usage::{DiscoveryContext, Source, UsageService, validate_report};

fn usage() -> Value {
    json!({
        "generatedAt":1750000000000u64,
        "reports":[
            {"provider":"anthropic","fetchedAt":1750000000000u64,
             "metadata":{"credentialId":4,"email":"dev@example.test","orgId":"work","orgName":"Work",
                         "planType":"max","subscriptionPlan":"Max 20x","accessToken":"private-metadata"},
             "limits":[{"id":"five_hour","label":"5 Hour","scope":{"provider":"anthropic"},
                         "window":{"id":"5h","label":"5 Hour","durationMs":18000000,"resetsAt":1750000010000u64},
                         "amount":{"usedFraction":0.5,"used":51,"limit":100,"unit":"percent"},"status":"ok"}],
             "raw":{"private":"private-raw"}},
            {"provider":"openai-codex","fetchedAt":1750000000000u64,
             "metadata":{"credentialId":5,"email":"dev@example.test","orgId":"chat","planType":"plus"},
             "limits":[{"id":"weekly","label":"Weekly","scope":{"provider":"openai-codex"},
                         "amount":{"remainingFraction":0.05,"unit":"percent"}}]},
            {"provider":"kiro","fetchedAt":1750000000000u64,
             "metadata":{"credentialId":6,"accountId":"native-account","planType":"Kiro Pro"},
             "limits":[{"id":"monthly","label":"Monthly credits","scope":{"provider":"kiro"},
                         "window":{"id":"monthly","label":"Monthly","resetsAt":1750000010000u64},
                         "amount":{"used":12.5,"limit":100.0,"unit":"credits"},"status":"ok"},
                        {"id":"reserve","label":"Reserve","scope":{"provider":"kiro"},
                         "amount":{"remaining":15.75,"unit":"credits"}}]},
            {"provider":"kiro-http","metadata":{"credentialId":7},"limits":[]},
            {"provider":"kiro-work-key","metadata":{"credentialId":8},"limits":[]}
        ],
        "accountsWithoutUsage":[{"type":"oauth","provider":"anthropic","email":"dev@example.test","orgId":"personal"},
                                {"type":"api_key","provider":"openrouter","key":"private-api-key"}],
        "disabledCredentials":[{"id":5,"type":"oauth","provider":"anthropic","email":"expired@example.test","cause":"private-cause"}],
        "capacity":{}
    })
}

fn accounts() -> Value {
    json!({"accounts":[
        {"provider":"anthropic","identityKey":"email:dev@example.test|org:work","orgName":"Work"},
        {"provider":"anthropic","identityKey":"email:dev@example.test|org:personal"},
        {"provider":"openai-codex","identityKey":"email:dev@example.test|org:chat"},
        {"provider":"kiro","identityKey":"account:native-account"},
        {"provider":"github-models","identityKey":"account:octocat"},
        {"provider":"unmeasured","identityKey":null}
    ]})
}

#[test]
fn omp_projects_both_cli_versions_without_forwarding_provider_data() {
    let payload = usage();
    let rows = normalize::project(
        serde_json::from_value(payload.clone()).unwrap(),
        Some(serde_json::from_value(accounts()).unwrap()),
        "fixture",
        None,
    );
    assert_eq!(rows.len(), 8);
    let get = |label: &str| &rows.values().find(|row| row.label == label).unwrap().report;
    let anthropic = get("OMP · anthropic · dev@example.test · Work");
    assert_eq!(anthropic["planLabel"], "Max 20x");
    assert_eq!(anthropic["windows"][0]["usedPct"], 50.0);
    assert_eq!(
        anthropic["windows"][0]["resetsAt"],
        "2025-06-15T15:06:50.000Z"
    );
    assert_eq!(
        get("OMP · openai-codex · dev@example.test · chat")["planLabel"],
        "plus"
    );
    let kiro = get("OMP · kiro · native-account");
    assert_eq!(kiro["windows"][0]["usedPct"], 12.5);
    assert_eq!(kiro["balances"][0]["used"], 12.5);
    assert_eq!(kiro["balances"][0]["limit"], 100.0);
    assert_eq!(kiro["balances"][0]["remaining"], 87.5);
    assert_eq!(kiro["balances"][1]["remaining"], 15.75);
    assert_eq!(
        get("OMP · github-models · octocat")["status"],
        "unavailable"
    );
    assert_eq!(get("OMP · openrouter · API key")["status"], "unavailable");
    assert!(
        get("OMP · anthropic · expired@example.test")["problem"]["detail"]
            .as_str()
            .unwrap()
            .contains("Sign in again")
    );
    for row in rows.values() {
        assert!(!row.label.contains("private-"));
        let report = validate_report(row.report.clone()).unwrap();
        assert!(!report.to_string().contains("private-"));
    }
    let mut installed = payload;
    installed["reports"][0]["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("subscriptionPlan");
    let installed = normalize::project(
        serde_json::from_value(installed).unwrap(),
        None,
        "fixture",
        Some("anthropic"),
    );
    assert_eq!(
        installed
            .values()
            .find(|row| row.report["status"] == "available")
            .unwrap()
            .report["planLabel"],
        "max"
    );
}

#[cfg(unix)]
fn spec(
    id: &str,
    script: &Path,
    command_env: impl IntoIterator<Item = (String, String)>,
) -> ProviderSpec {
    ProviderSpec {
        id: id.into(),
        family: "omp".into(),
        command: vec![
            "/bin/sh".into(),
            script.to_string_lossy().into_owned(),
            "--profile".into(),
            "fixture".into(),
            "--mode=rpc-ui".into(),
        ],
        env: command_env.into_iter().collect(),
        options: json!({}),
        defaults: json!({}),
        runtime_settings: json!({}),
        configured_models: Vec::new(),
    }
}

#[cfg(unix)]
fn fixture_files(path: &Path, report: &Value) {
    std::fs::write(path.join("usage.json"), report.to_string()).unwrap();
    std::fs::write(path.join("accounts.json"), accounts().to_string()).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn omp_fake_cli_uses_launch_settings_and_refreshes_agent_inventory() {
    let home = tempfile::tempdir().unwrap();
    let agent_home = home.path().join("agent");
    std::fs::create_dir(&agent_home).unwrap();
    let script = home.path().join("fixture.sh");
    std::fs::write(
        &script,
        r#"
set -eu
[ "${HOME:?}" = "$PWD" ]
[ "${PASEO_NODE_ENV-unset}" = unset ]
[ "$1" = --profile ] && [ "$2" = fixture ] && [ "$3" = usage ]
shift 3
kind=usage
if [ "$1" = accounts ]; then kind=accounts; shift; fi
[ "$1" = --json ] && [ "$2" = --no-extensions ]
shift 2
if [ "${EXPECTED_PROVIDER-}" ]; then
  [ "$1" = --provider ] && [ "$2" = "$EXPECTED_PROVIDER" ]
  shift 2
fi
[ "$#" = 0 ]
printf '%s\n' "$kind" >> calls
/bin/cat "$kind.json"
"#,
    )
    .unwrap();
    fixture_files(home.path(), &usage());
    fixture_files(&agent_home, &usage());
    let session = json!({"agentId":"agent","provider":"custom-omp","model":"kiro/auto","sessionKey":"session",
        "cwd":agent_home,"env":{"HOME":agent_home,"EXPECTED_PROVIDER":"kiro","OMP_KIRO_TRANSPORT":"http"}});
    let context = UsageContext {
        env: BTreeMap::from([(
            "HOME".into(),
            "/global-env-does-not-override-provider".into(),
        )]),
        user_home: Some(home.path().into()),
        provider_specs: vec![
            spec(
                "omp",
                &script,
                [
                    ("HOME".into(), home.path().to_string_lossy().into_owned()),
                    ("PASEO_NODE_ENV".into(), "private-wrapper-env".into()),
                ],
            ),
            spec("custom-omp", &script, []),
        ],
        sessions: vec![session],
        muse_providers: Vec::new(),
        plugins: None,
    };
    let service = UsageService::new(home.path().into());
    let rt = service.runtime(&context, CancellationToken::new(), None);
    let scope = UsageScope {
        provider: None,
        model: None,
        env: BTreeMap::new(),
    };
    let result = service
        .discover_and_fetch(
            DiscoveryContext {
                runtime: &rt,
                scope: &scope,
                agent: None,
            },
            &[Source::Omp],
            true,
            &rt.cancel,
            None,
        )
        .await
        .unwrap();
    assert!(result.unavailable_sources.is_empty());
    assert_eq!(result.entries.as_array().unwrap().len(), 8);
    assert_eq!(
        std::fs::read_to_string(home.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    let request = json!({"agentId":"agent","forceRefresh":true});
    let entries = service
        .list_reports(&request, &context, CancellationToken::new())
        .await
        .unwrap();
    let entries = entries.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["sourceId"], "omp");
    assert_eq!(
        entries[0]["account"]["label"],
        "OMP · kiro · native-account"
    );
    assert_eq!(entries[0]["report"]["planLabel"], "Kiro Pro");
    let mut changed = usage();
    changed["reports"].as_array_mut().unwrap()[2]["metadata"]["accountId"] = json!("other-account");
    std::fs::write(agent_home.join("usage.json"), changed.to_string()).unwrap();
    std::fs::write(
        agent_home.join("accounts.json"),
        r#"{"accounts":[{"provider":"kiro","identityKey":"account:other-account"}]}"#,
    )
    .unwrap();
    let refreshed = service
        .list_reports(&request, &context, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        refreshed[0]["account"]["label"],
        "OMP · kiro · other-account"
    );
    assert_eq!(
        std::fs::read_to_string(agent_home.join("calls"))
            .unwrap()
            .lines()
            .count(),
        4
    );
}

#[cfg(unix)]
#[tokio::test]
async fn omp_unavailable_cli_failures_hide_raw_output() {
    let dir = tempfile::tempdir().unwrap();
    let command = |program: String, args: Vec<String>| {
        let prefix = std::iter::once(program).chain(args).collect::<Vec<_>>();
        Launch::new(&prefix, std::env::vars().collect(), dir.path().into(), None)
    };
    let missing = command(
        dir.path()
            .join("nonexistent")
            .to_string_lossy()
            .into_owned(),
        Vec::new(),
    );
    let cancel = CancellationToken::new();
    assert!(
        missing
            .rows(&cancel)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("not found")
    );
    let old = command(
        "/bin/sh".into(),
        vec![
            "-c".into(),
            "printf '%s' private-output; printf '%s' private-error >&2; exit 0".into(),
        ],
    );
    let error = old.rows(&cancel).await.err().unwrap().to_string();
    assert!(error.contains("recognized usage JSON"));
    assert!(!error.contains("private-"));
    let oversized = command(
        "/bin/sh".into(),
        vec!["-c".into(), "head -c 4194305 /dev/zero".into()],
    );
    assert!(
        oversized
            .rows(&cancel)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("exceeded its output limit")
    );
    cancel.cancel();
    let cancelled = command(
        "/bin/sh".into(),
        vec!["-c".into(), "touch not-spawned".into()],
    );
    assert!(
        cancelled
            .rows(&cancel)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("cancelled")
    );
    assert!(!dir.path().join("not-spawned").exists());
}
