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
    let installed = normalize::project(serde_json::from_value(installed).unwrap(), None, "fixture");
    assert_eq!(
        installed
            .values()
            .find(|row| row.provider == "anthropic" && row.report["status"] == "available")
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
[ "$#" = 0 ]
printf '%s\n' "$kind" >> calls
/bin/cat "$kind.json"
"#,
    )
    .unwrap();
    fixture_files(home.path(), &usage());
    fixture_files(&agent_home, &usage());
    let snapshot = spec("custom-omp", &script, []).command;
    let session = json!({"agentId":"agent","provider":"custom-omp","providerFamily":"omp","model":"kiro/auto","sessionKey":"session",
        "command":snapshot,"cwd":agent_home,"env":{"HOME":agent_home,"OMP_KIRO_TRANSPORT":"http"}});
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
            spec(
                "custom-omp",
                &home.path().join("changed-config-never-called.sh"),
                [],
            ),
        ],
        enabled_provider_ids: HashSet::from(["omp".into()]),
        sessions: vec![session],
        muse_providers: Vec::new(),
        plugins: None,
    };
    let service = UsageService::new(home.path().into());
    let rt = service.runtime(&context, CancellationToken::new(), None, true);
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
    assert_eq!(entries[0]["fetchedAt"], "2025-06-15T15:06:40.000Z");
    assert_eq!(entries[0]["successfulAt"], entries[0]["fetchedAt"]);
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
        Launch::new(
            &prefix,
            std::env::vars().collect(),
            dir.path().into(),
            dir.path(),
        )
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
            .collect(&cancel)
            .await
            .err()
            .unwrap()
            .contains("not found")
    );
    let old = command(
        "/bin/sh".into(),
        vec![
            "-c".into(),
            "printf '%s' private-output; printf '%s' private-error >&2; exit 0".into(),
        ],
    );
    let error = old.collect(&cancel).await.err().unwrap();
    assert!(error.contains("recognized usage JSON"));
    assert!(!error.contains("private-"));
    let oversized = command(
        "/bin/sh".into(),
        vec!["-c".into(), "head -c 4194305 /dev/zero".into()],
    );
    assert!(
        oversized
            .collect(&cancel)
            .await
            .err()
            .unwrap()
            .contains("exceeded its output limit")
    );
    cancel.cancel();
    let cancelled = command(
        "/bin/sh".into(),
        vec!["-c".into(), "touch not-spawned".into()],
    );
    assert!(
        cancelled
            .collect(&cancel)
            .await
            .err()
            .unwrap()
            .contains("cancelled")
    );
    assert!(!dir.path().join("not-spawned").exists());
}

#[cfg(unix)]
fn fixture_context(home: &Path, provider_specs: Vec<ProviderSpec>) -> UsageContext {
    UsageContext {
        env: BTreeMap::from([("HOME".into(), home.to_string_lossy().into_owned())]),
        user_home: Some(home.into()),
        enabled_provider_ids: provider_specs.iter().map(|spec| spec.id.clone()).collect(),
        provider_specs,
        sessions: Vec::new(),
        muse_providers: Vec::new(),
        plugins: None,
    }
}

#[cfg(unix)]
async fn only_omp(
    service: &UsageService,
    context: &UsageContext,
    force: bool,
) -> crate::usage::UsageReports {
    let rt = service.runtime(context, CancellationToken::new(), None, force);
    service
        .discover_and_fetch(
            DiscoveryContext {
                runtime: &rt,
                scope: &UsageScope {
                    provider: None,
                    model: None,
                    env: BTreeMap::new(),
                },
                agent: None,
            },
            &[Source::Omp],
            force,
            &rt.cancel,
            None,
        )
        .await
        .unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn omp_optional_cli_absence_and_disabled_provider_are_complete() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let forbidden = home.join("forbidden.sh");
    std::fs::write(&forbidden, "/bin/touch must-not-spawn\nexit 42").unwrap();
    let mut missing = spec("custom-missing-omp", &forbidden, []);
    missing.command = vec!["missing-omp".into()];
    missing
        .env
        .insert("PATH".into(), home.to_str().unwrap().to_owned());
    let disabled = spec("omp", &forbidden, []);
    let mut context = fixture_context(home, vec![disabled, missing]);
    context.enabled_provider_ids.remove("omp");
    let service = UsageService::new(home.into());
    let reports = only_omp(&service, &context, true).await;
    assert!(reports.unavailable_sources.is_empty());
    assert_eq!(reports.entries, json!([]));
    assert!(!home.join("must-not-spawn").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn omp_global_discovers_distinct_custom_stores_once_and_keeps_partial_results() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = home.join("fixture.sh");
    std::fs::write(
        &script,
        r#"
        set -eu
        if [ "$1" = --profile ]; then shift 2; fi
        [ "$1" = usage ]; shift
        printf '%s\n' "$1" >> "$PROBE"
        if [ "${BROKEN-}" ]; then exit 22; fi
        if [ "$1" = accounts ]; then /bin/cat accounts.json; else /bin/cat usage.json; fi
    "#,
    )
    .unwrap();
    let provider = |id: &str| spec(id, &script, [("PROBE".into(), id.into())]);
    let base = provider("omp");
    let mut same = provider("alias");
    same.command.drain(2..4);
    same.env.insert("OMP_PROFILE".into(), "fixture".into());
    let mut second = provider("second");
    second.command[3] = "other-store".into();
    let mut broken = provider("broken");
    broken.command[3] = "broken-store".into();
    broken.env.insert("BROKEN".into(), "1".into());
    let context = fixture_context(home, vec![base, same, second, broken]);
    fixture_files(home, &usage());
    let service = UsageService::new(home.into());
    let reports = only_omp(&service, &context, true).await;
    assert_eq!(reports.unavailable_sources, vec!["omp"]);
    for id in ["omp", "second", "broken"] {
        assert_eq!(
            std::fs::read_to_string(home.join(id))
                .unwrap()
                .lines()
                .count(),
            2
        );
    }
    assert!(!home.join("alias").exists());
    let rows = reports.entries.as_array().unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| row["account"]["label"] == "OMP · openrouter · API key")
            .count(),
        2
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row["account"]["label"] == "OMP · kiro · native-account")
            .count(),
        1
    );
}

#[test]
fn omp_anonymous_ids_follow_the_resolved_agent_dir_profile_and_broker() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let first = home.join("first");
    let second = home.join("second");
    std::fs::create_dir_all(first.join("relative-agent")).unwrap();
    std::fs::create_dir_all(second.join("relative-agent")).unwrap();
    let mut env = BTreeMap::from([("PI_CODING_AGENT_DIR".into(), "relative-agent".into())]);
    let id = |env: &BTreeMap<String, String>, cwd: &Path, args: &[String]| {
        normalize::project(
            serde_json::from_value(json!({"reports":[],"accountsWithoutUsage":[{"provider":"openrouter","type":"api_key","id":7}]})).unwrap(),
            None,
            &store_namespace(args, env, cwd, home),
        ).into_keys().next().unwrap()
    };
    assert_ne!(id(&env, &first, &[]), id(&env, &second, &[]));
    let a = id(&env, &first, &[]);
    env.insert(
        "PI_CODING_AGENT_DIR".into(),
        first.join("relative-agent").to_string_lossy().into_owned(),
    );
    assert_eq!(a, id(&env, &second, &[]));
    let profile = id(&env, &first, &["--profile=one".into()]);
    assert_ne!(profile, id(&env, &first, &["--profile=two".into()]));
    env.insert("OMP_PROFILE".into(), "one".into());
    assert_eq!(profile, id(&env, &second, &[]));
    env.insert(
        "OMP_AUTH_BROKER_URL".into(),
        "https://first.example.test".into(),
    );
    env.insert(
        "OMP_AUTH_BROKER_TOKEN".into(),
        "private-broker-token".into(),
    );
    let broker = id(&env, &first, &[]);
    assert_ne!(profile, broker);
    env.insert(
        "OMP_AUTH_BROKER_URL".into(),
        "https://second.example.test".into(),
    );
    assert_ne!(broker, id(&env, &first, &[]));

    env.remove("OMP_AUTH_BROKER_URL");
    env.remove("OMP_AUTH_BROKER_TOKEN");
    let settings = home.join(".omp/profiles/one/agent");
    std::fs::create_dir_all(&settings).unwrap();
    std::fs::write(
        settings.join("config.yml"),
        "auth:\n  broker:\n    url: FIXTURE_BROKER_URL\n    token: private-broker-token\n",
    )
    .unwrap();
    env.insert(
        "FIXTURE_BROKER_URL".into(),
        "https://first.example.test".into(),
    );
    assert_eq!(broker, id(&env, &first, &[]));
    env.insert(
        "FIXTURE_BROKER_URL".into(),
        "https://third.example.test".into(),
    );
    assert_ne!(broker, id(&env, &first, &[]));
}

#[test]
fn omp_anonymous_inventory_does_not_duplicate_identified_or_ambiguous_rows() {
    let rows = normalize::project(
        serde_json::from_value(json!({"reports":[{
            "provider":"github-copilot","metadata":{"accountId":"known-account","credentialId":7},
            "limits":[{"id":"quota","label":"Quota","scope":{},"amount":{"usedFraction":0.2,"unit":"percent"}}]
        }],"accountsWithoutUsage":[{"provider":"github-copilot","id":7,"type":"oauth"}]})).unwrap(),
        Some(serde_json::from_value(json!({"accounts":[
            {"provider":"github-copilot","identityKey":null},
            {"provider":"ambiguous","identityKey":null},
            {"provider":"ambiguous","identityKey":null}
        ]})).unwrap()),
        "store",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows.values().next().unwrap().label,
        "OMP · github-copilot · known-account"
    );
    assert_eq!(rows.values().next().unwrap().report["status"], "available");
}

#[cfg(unix)]
#[tokio::test]
async fn omp_concurrent_scoped_and_global_reads_share_batch_but_force_refreshes_once() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = home.join("fixture.sh");
    std::fs::write(
        &script,
        r#"
        set -eu
        [ "$1" = --profile ] && [ "$2" = fixture ] && [ "$3" = usage ]
        shift 3
        kind=usage
        if [ "$1" = accounts ]; then kind=accounts; shift; fi
        [ "$1" = --json ] && [ "$2" = --no-extensions ] && [ "$#" = 2 ]
        printf '%s\n' "$kind" >> calls
        /bin/sleep 0.05
        /bin/cat "$kind.json"
    "#,
    )
    .unwrap();
    let mut context = fixture_context(home, vec![spec("omp", &script, [])]);
    context.sessions.push(json!({"agentId":"agent","provider":"omp","providerFamily":"omp","model":"kiro/auto","sessionKey":"session",
        "command":context.provider_specs[0].command,"cwd":home,"env":context.env}));
    fixture_files(home, &usage());
    let service = UsageService::new(home.into());
    let scoped = json!({"agentId":"agent"});
    let (global, scoped_entries) = tokio::join!(
        only_omp(&service, &context, false),
        service.list_reports(&scoped, &context, CancellationToken::new()),
    );
    assert!(global.unavailable_sources.is_empty());
    assert_eq!(scoped_entries.unwrap().as_array().unwrap().len(), 1);
    let read_calls = || {
        std::fs::read_to_string(home.join("calls"))
            .unwrap()
            .lines()
            .count()
    };
    assert_eq!(read_calls(), 2);
    let fresh = only_omp(&service, &context, false).await;
    let project = |entries: &Value| {
        entries
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                json!([
                    row["id"],
                    row["account"],
                    row["report"],
                    row["successfulAt"]
                ])
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(project(&fresh.entries), project(&global.entries));
    assert_eq!(read_calls(), 2);
    let ids: Vec<_> = global
        .entries
        .as_array()
        .unwrap()
        .iter()
        .map(|row| &row["id"])
        .collect();
    let mut changed = usage();
    changed["reports"][2]["fetchedAt"] = json!(1750000005000u64);
    fixture_files(home, &changed);
    let reports = service
        .list_reports(
            &json!({"reportIds":ids,"forceRefresh":true}),
            &context,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let kiro = reports
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["account"]["label"] == "OMP · kiro · native-account")
        .unwrap();
    assert_eq!(kiro["successfulAt"], "2025-06-15T15:06:45.000Z");
    assert_eq!(read_calls(), 4);
    let scoped_entries = service
        .list_reports(&scoped, &context, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(scoped_entries[0]["successfulAt"], kiro["successfulAt"]);
    assert_eq!(read_calls(), 4);
}
