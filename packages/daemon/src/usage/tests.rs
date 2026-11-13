use super::*;

#[test]
fn canonical_login_identity_preserves_ordered_fallbacks() {
    let first = Login {
        harness: "Codex".into(),
        input: json!({"route":{"store":"codex","path":"/fixture/auth.json"}}),
    };
    let reordered = Login {
        harness: "Codex".into(),
        input: json!({"route":{"path":"/fixture/auth.json","store":"codex"}}),
    };
    let second = Login {
        harness: "Pi".into(),
        input: json!({"route":{"store":"pi","path":"/fixture/pi.json"}}),
    };
    assert_eq!(login_key(&first), login_key(&reordered));
    let mut report = KnownReport {
        source: Source::Codex,
        label: None,
        logins: vec![first, second],
    };
    let original = cache_identity("codex:account", &report);
    report.logins.reverse();
    assert_ne!(original, cache_identity("codex:account", &report));
}

#[test]
fn global_merge_preserves_each_sessions_fallback_and_cache_identity() {
    let login = |harness: &str, path: &str| Login {
        harness: harness.into(),
        input: json!({"route":{"store":"codex","path":path}}),
    };
    let report = |login| KnownReport {
        source: Source::Codex,
        label: Some("Account".into()),
        logins: vec![login],
    };
    let global = report(login("Codex", "/global/auth.json"));
    let session = report(login("Codex", "/session/auth.json"));
    let mut state = State::default();
    state
        .defaults
        .push(("codex:account".into(), global.clone()));
    state.by_agent.push((
        "agent".into(),
        AgentReports {
            signature: json!(["codex", null, "launch"]),
            reports: vec![("codex:account".into(), session.clone())],
            complete: true,
        },
    ));
    let merged = merged_reports(&state);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].1.logins.len(), 2);
    let active = active_cache_keys(&state);
    assert!(active.contains(&cache_identity("codex:account", &global)));
    assert!(active.contains(&cache_identity("codex:account", &session)));
    assert!(active.contains(&cache_identity("codex:account", &merged[0].1)));
    state.by_agent.clear();
    assert!(!active_cache_keys(&state).contains(&cache_identity("codex:account", &session)));
}

#[test]
fn reported_duration_does_not_invent_a_window_length() {
    let unknown = common::duration_window(
        None,
        None,
        ("primary", "Current window", ""),
        Some(25.0),
        None,
        true,
        Some("ok"),
    );
    assert_eq!(
        unknown,
        json!({"id":"primary","label":"Current window","shortLabel":"","usedPct":25.0,
        "remainingPct":75.0,"resetsAt":null,"summary":true,"tone":"ok"})
    );
    let weekly = common::duration_window(
        Some(604800.0),
        Some(("model", "Model")),
        ("unknown", "Unknown", ""),
        Some(105.0),
        None,
        false,
        None,
    );
    assert_eq!(weekly["id"], "model:weekly");
    assert_eq!(weekly["shortLabel"], "Model wk");
    assert_eq!(weekly["remainingPct"], 0.0);
    assert_eq!(common::tone(Some(90.0)), "warning");
    assert_eq!(common::tone(Some(90.1)), "danger");
}

#[test]
fn api_coercion_matches_javascript_scalars_and_arrays() {
    assert_eq!(common::api_number(&json!(null)), Some(0.0));
    assert_eq!(common::api_number(&json!(" 0x10 ")), Some(16.0));
    assert_eq!(common::api_number(&json!([])), Some(0.0));
    assert_eq!(common::api_number(&json!(["7"])), Some(7.0));
    assert_eq!(common::api_number(&json!([1, 2])), None);
    assert_eq!(common::api_number(&json!("Infinity")), None);
    assert_eq!(common::api_string(&json!(1.0)), "1");
    assert_eq!(common::api_string(&json!(-0.0)), "0");
    assert_eq!(common::api_string(&json!([1.0, null, "x"])), "1,,x");
}

#[test]
fn reloaded_plugin_source_cannot_reuse_previous_worker_cache() {
    let source = |generation: &str| Source::Plugin {
        id: "fixture".into(),
        label: "Fixture".into(),
        icon: None,
        plugin_id: "fixture-plugin".into(),
        generation: generation.into(),
    };
    let report = |generation| KnownReport {
        source: source(generation),
        label: None,
        logins: vec![Login {
            harness: "Fixture".into(),
            input: json!({"locator":"same-store"}),
        }],
    };
    assert_ne!(
        cache_identity("fixture:account", &report("first")),
        cache_identity("fixture:account", &report("second"))
    );
}

#[tokio::test]
async fn builtin_kiro_reports_expiry_without_an_external_plugin_or_auth_writes() {
    let home = tempfile::tempdir().unwrap();
    let auth = home.path().join("auth.sqlite3");
    let db = rusqlite::Connection::open(&auth).unwrap();
    db.execute_batch("CREATE TABLE credentials (id TEXT PRIMARY KEY, value TEXT NOT NULL)").unwrap();
    db.execute(
        "INSERT INTO credentials VALUES ('active', ?1)",
        [json!({
            "kind":"social","provider":"github","access_token":"private-fixture-token",
            "expires_at":"2000-01-01T00:00:00Z"
        }).to_string()],
    ).unwrap();
    drop(db);
    let before = std::fs::read(&auth).unwrap();
    let context = UsageContext {
        env: BTreeMap::from([("OMP_KIRO_AUTH_FILE".into(), auth.to_str().unwrap().into())]),
        user_home: Some(home.path().into()),
        provider_specs: Vec::new(),
        sessions: Vec::new(),
        muse_providers: Vec::new(),
        plugins: None,
    };
    let service = UsageService::new(home.path().into());
    let runtime = service.runtime(&context, CancellationToken::new());
    let sources: Vec<_> = registered_sources(&runtime).unwrap().into_iter()
        .filter(|source| matches!(source, Source::Kiro)).collect();
    let scope = UsageScope { provider: None, model: None, env: BTreeMap::new() };
    let result = service.discover_and_fetch(
        &runtime, &scope, &sources, None, true, &runtime.cancel, None,
    ).await.unwrap();
    let expected_id = format!("kiro:{}", hash_account_key(auth.canonicalize().unwrap().to_str().unwrap()));
    assert_eq!(result.entries[0]["id"], expected_id);
    assert_eq!(result.entries[0]["report"], json!({
        "status":"unavailable","problem":{
            "kind":"expired","expiresAt":"2000-01-01T00:00:00.000Z","refreshedBy":"omp-kiro"
        }
    }));
    assert!(!result.entries.to_string().contains("private-fixture-token"));
    assert_eq!(std::fs::read(auth).unwrap(), before);
}
