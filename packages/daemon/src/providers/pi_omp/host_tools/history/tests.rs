use super::*;
use serde_json::json;
use std::{io::Write, sync::Arc};

fn session(path: &Path, calls: Value) -> Result<()> {
    fs::write(
        path,
        format!(
            "{}\n",
            json!({"type":"message","message":{"role":"assistant","content":calls}})
        ),
    )?;
    Ok(())
}

fn timeline(home: &Path, items: &[Value]) -> Result<PathBuf> {
    let directory = home.join("agent-timelines");
    fs::create_dir_all(&directory)?;
    let path = directory.join("agent.sqlite3");
    let database = Connection::open(&path)?;
    database.execute_batch(
        "PRAGMA journal_mode=WAL;
        CREATE TABLE timeline_metadata(id INTEGER PRIMARY KEY, epoch TEXT, next_seq INTEGER);
        CREATE TABLE timeline_rows(position INTEGER PRIMARY KEY,row_json TEXT);",
    )?;
    database.execute(
        "INSERT INTO timeline_metadata VALUES(1,'epoch',?1)",
        [items.len() as i64 + 1],
    )?;
    for (position, item) in items.iter().enumerate() {
        database.execute(
            "INSERT INTO timeline_rows VALUES(?1,?2)",
            params![
                position as i64,
                json!({"seqEnd":position+1,"item":item}).to_string()
            ],
        )?;
    }
    Ok(path)
}

#[tokio::test]
async fn bootstrap_counts_failed_calls_and_excludes_schema_reads_and_mentions() -> Result<()> {
    let home = tempfile::tempdir()?;
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions)?;
    timeline(
        home.path(),
        &[
            json!({"type":"tool_call","callId":"failed","name":"functions.used","status":"failed","detail":{"type":"unknown","input":{}}}),
            json!({"type":"assistant_message","text":"functions.mentioned xd://mentioned"}),
            json!({"type":"tool_call","callId":"schema","name":"schema_only","detail":{"type":"plain_text","label":"schema_only"}}),
        ],
    )?;
    session(
        &sessions.join("native.jsonl"),
        json!([
            {"type":"toolCall","id":"read","name":"functions.read","arguments":{"path":"xd://schema_only"}},
            {"type":"toolCall","id":"execute","name":"functions.write","arguments":{"path":"xd://executed","content":"{}"}},
            {"type":"text","text":"toolCall name mentioned"}
        ]),
    )?;
    let history = History::load(home.path(), vec![sessions]).await;
    assert!(history.complete);
    let mut catalog = Catalog::default();
    for name in ["used", "executed", "schema_only", "mentioned", "unseen"] {
        catalog.insert(name, None);
    }
    assert!(catalog.essential("used", &history));
    assert!(catalog.essential("executed", &history));
    for name in ["schema_only", "mentioned", "unseen"] {
        assert!(!catalog.essential(name, &history));
    }
    Ok(())
}

#[tokio::test]
async fn exact_server_identity_and_ambiguous_aliases_do_not_cross_promote() -> Result<()> {
    let home = tempfile::tempdir()?;
    let first = super::super::bridge_name("first", "search");
    let second = super::super::bridge_name("second", "search");
    let mut catalog = Catalog::default();
    catalog.insert(&first, Some(("first", "search")));
    catalog.insert(&second, Some(("second", "search")));
    catalog.insert("search", None);
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions)?;
    session(
        &sessions.join("native.jsonl"),
        json!([
            {"type":"toolCall","id":"a","name":"functions.mcp__first__search","arguments":{}}
        ]),
    )?;
    let history = History::load(home.path(), vec![sessions.clone()]).await;
    assert!(catalog.essential(&first, &history));
    assert!(!catalog.essential(&second, &history));
    assert!(!catalog.essential("search", &history));
    // An exact native name colliding with a legacy MCP alias is ambiguous.
    catalog.insert("mcp__first__search", None);
    assert!(!catalog.essential(&first, &history));
    assert!(!catalog.essential("mcp__first__search", &history));
    history.record(catalog.identity(&first).unwrap()).await?;
    let reloaded = History::load(home.path(), vec![sessions]).await;
    assert!(catalog.essential(&first, &reloaded));
    assert!(!catalog.essential("mcp__first__search", &reloaded));
    Ok(())
}

#[tokio::test]
async fn concurrent_persist_reload_and_next_launch_keep_current_catalog_fixed() -> Result<()> {
    let home = tempfile::tempdir()?;
    let (first, second) = tokio::join!(
        History::load(home.path(), vec![]),
        History::load(home.path(), vec![])
    );
    let mut catalog = Catalog::default();
    catalog.insert("one", None);
    catalog.insert("two", None);
    assert!(!catalog.essential("one", &first));
    let first = Arc::new(first);
    let second = Arc::new(second);
    let (a, b, duplicate) = tokio::join!(
        first.record(catalog.identity("one").unwrap()),
        second.record(catalog.identity("two").unwrap()),
        second.record(catalog.identity("one").unwrap()),
    );
    a?;
    b?;
    duplicate?;
    assert!(!catalog.essential("one", &first));
    let reloaded = History::load(home.path(), vec![]).await;
    assert!(catalog.essential("one", &reloaded));
    assert!(catalog.essential("two", &reloaded));
    Ok(())
}

#[tokio::test]
async fn incremental_sources_include_updated_old_rows_and_appended_calls() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = timeline(
        home.path(),
        &[json!({"type":"tool_call","callId":"a","name":"old","detail":{"type":"unknown"}})],
    )?;
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions)?;
    let native = sessions.join("native.jsonl");
    session(
        &native,
        json!([{ "type":"toolCall","id":"a","name":"initial","arguments":{} }]),
    )?;
    let first = History::load(home.path(), vec![sessions.clone()]).await;
    assert!(first.complete);
    let database = Connection::open(path)?;
    database.execute("UPDATE timeline_rows SET row_json=?1 WHERE position=0", [json!({"seqEnd":2,"item":{"type":"tool_call","callId":"a","name":"updated","detail":{"type":"unknown"}}}).to_string()])?;
    database.execute("UPDATE timeline_metadata SET next_seq=3", [])?;
    let mut writer = fs::OpenOptions::new().append(true).open(&native)?;
    writeln!(
        writer,
        "{}",
        json!({"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","id":"b","name":"appended","arguments":{}}]}})
    )?;
    let second = History::load(home.path(), vec![sessions.clone()]).await;
    assert!(second.complete);
    for name in ["old", "initial", "updated", "appended"] {
        assert!(second.names.contains(name));
    }
    // Replacing/truncating a native session does not strand its old offset.
    session(
        &native,
        json!([{ "type":"toolCall","id":"x","name":"replacement","arguments":{} }]),
    )?;
    let replaced = History::load(home.path(), vec![sessions]).await;
    assert!(replaced.names.contains("replacement"));
    assert!(replaced.names.contains("appended"));
    Ok(())
}

#[tokio::test]
async fn missing_history_is_empty_but_unreadable_history_is_explicitly_partial() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut catalog = Catalog::default();
    catalog.insert("unseen", None);
    let empty = History::load(home.path(), vec![home.path().join("missing")]).await;
    assert!(empty.complete);
    assert!(!catalog.essential("unseen", &empty));
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions)?;
    fs::write(sessions.join("broken.jsonl"), "not json\n")?;
    let partial = History::load(home.path(), vec![sessions.clone()]).await;
    assert!(!partial.complete);
    assert!(catalog.essential("unseen", &partial));
    assert!(partial.names.is_empty());
    assert!(partial.identities.is_empty());
    session(&sessions.join("broken.jsonl"), json!([]))?;
    let repaired = History::load(home.path(), vec![sessions]).await;
    assert!(repaired.complete);
    assert!(!catalog.essential("unseen", &repaired));
    // A non-directory source is not an absent-history success.
    let not_directory = home.path().join("not-directory");
    fs::write(&not_directory, "")?;
    assert!(
        !History::load(home.path(), vec![not_directory])
            .await
            .complete
    );
    Ok(())
}

#[tokio::test]
async fn trailing_partial_record_retries_without_losing_committed_calls() -> Result<()> {
    let home = tempfile::tempdir()?;
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions)?;
    let file = sessions.join("native.jsonl");
    session(
        &file,
        json!([{ "type":"toolCall","id":"a","name":"committed","arguments":{} }]),
    )?;
    let pending = json!({"type":"message","message":{"role":"assistant","content":[{"type":"toolCall","id":"b","name":"pending","arguments":{}}]}}).to_string();
    let mut writer = fs::OpenOptions::new().append(true).open(&file)?;
    write!(writer, "{pending}")?;
    let partial = History::load(home.path(), vec![sessions.clone()]).await;
    assert!(!partial.complete);
    assert!(partial.names.contains("committed"));
    assert!(!partial.names.contains("pending"));
    writeln!(writer)?;
    let completed = History::load(home.path(), vec![sessions]).await;
    assert!(completed.complete);
    assert!(completed.names.contains("pending"));
    Ok(())
}

#[test]
fn structured_parallel_execution_counts_only_actual_targets() {
    let mut names = HashSet::new();
    invocation(
        "multi_tool_use.parallel",
        &json!({"tool_uses":[
            {"recipient_name":"functions.write","parameters":{"path":"xd://mcp__server__execute","content":"{}"}},
            {"recipient_name":"functions.read","parameters":{"path":"xd://schema_only"}}
        ]}),
        &mut names,
    );
    assert!(names.contains("mcp__server__execute"));
    assert!(!names.contains("schema_only"));
    invocation("write", &json!({"path":"xd://nested/schema"}), &mut names);
    assert!(!names.contains("nested"));
}

#[tokio::test]
async fn hashed_bridge_and_omp_mcp_aliases_preserve_server_identity() -> Result<()> {
    let home = tempfile::tempdir()?;
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions)?;
    let one = super::super::bridge_name("one", "run");
    let two = super::super::bridge_name("two", "run");
    let unseen = super::super::bridge_name("three", "run");
    let mut catalog = Catalog::default();
    catalog.insert(&one, Some(("one", "run")));
    catalog.insert(&two, Some(("two", "run")));
    catalog.insert(&unseen, Some(("three", "run")));
    session(
        &sessions.join("native.jsonl"),
        json!([
            {"type":"toolCall","id":"a","name":one,"arguments":{}},
            {"type":"toolCall","id":"b","name":"write","arguments":{"path":"xd://mcp__two_run","content":"{}"}}
        ]),
    )?;
    let history = History::load(home.path(), vec![sessions]).await;
    assert!(catalog.essential(&one, &history));
    assert!(catalog.essential(&two, &history));
    assert!(!catalog.essential(&unseen, &history));
    Ok(())
}

#[tokio::test]
async fn corrupt_source_retains_valid_invocations_without_claiming_complete() -> Result<()> {
    let home = tempfile::tempdir()?;
    let sessions = home.path().join("sessions");
    fs::create_dir(&sessions)?;
    let file = sessions.join("native.jsonl");
    session(
        &file,
        json!([{ "type":"toolCall","id":"a","name":"retained","arguments":{} }]),
    )?;
    let mut writer = fs::OpenOptions::new().append(true).open(&file)?;
    writeln!(writer, "corrupt record")?;
    let history = History::load(home.path(), vec![sessions]).await;
    assert!(!history.complete);
    assert!(history.names.contains("retained"));
    drop(writer);
    fs::remove_file(file)?;
    let reloaded = History::load(home.path(), vec![]).await;
    assert!(reloaded.complete);
    assert!(reloaded.names.contains("retained"));
    Ok(())
}

#[test]
fn configured_and_default_omp_roots_are_included_with_resolved_override() {
    let env = std::collections::BTreeMap::from([
        ("HOME".into(), "/fixture/home".into()),
        ("PI_CODING_AGENT_DIR".into(), "~/custom-agent".into()),
    ]);
    let roots = session_roots(
        &env,
        Path::new("/fixture/work"),
        PathBuf::from("/fixture/override"),
    );
    assert!(roots.contains(&PathBuf::from("/fixture/home/.omp/agent/sessions")));
    assert!(roots.contains(&PathBuf::from("/fixture/home/custom-agent/sessions")));
    assert!(roots.contains(&PathBuf::from("/fixture/override")));
}

#[cfg(unix)]
#[tokio::test]
async fn new_history_cache_is_private() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir()?;
    let history = History::load(home.path(), vec![]).await;
    assert!(history.complete);
    assert_eq!(
        fs::metadata(&history.path)?.permissions().mode() & 0o777,
        0o600
    );
    Ok(())
}

#[test]
fn legacy_ambiguity_is_counted_separately_from_actual_invocations() {
    let mut names = HashSet::new();
    assert!(timeline_names(
        &json!({
            "type":"tool_call","callId":"schema","name":"target",
            "detail":{"type":"plain_text","label":"target"}
        }),
        &mut names
    ));
    assert!(names.is_empty());
    assert!(!timeline_names(
        &json!({
            "type":"tool_call","callId":"executed","name":"target",
            "detail":{"type":"unknown","input":{}}
        }),
        &mut names
    ));
    assert!(names.contains("target"));
}

#[tokio::test]
async fn resolved_provider_session_directory_participates_in_bootstrap() -> Result<()> {
    let home = tempfile::tempdir()?;
    let sessions = home.path().join("configured-sessions");
    fs::create_dir(&sessions)?;
    session(
        &sessions.join("native.jsonl"),
        json!([
            {"type":"toolCall","id":"configured","name":"configured_call","arguments":{}}
        ]),
    )?;
    let env = std::collections::BTreeMap::from([(
        "HOME".into(),
        home.path().to_string_lossy().into_owned(),
    )]);
    let options = json!({
        "cwd":home.path(),"homeDir":home.path(),
        "providerOptions":{"sessionDir":sessions}
    });
    let resolved = super::super::super::history::sessions_dir("omp", &env, &options).await?;
    let roots = session_roots(&env, home.path(), resolved);
    let history = History::load(home.path(), roots).await;
    assert!(history.complete);
    assert!(history.names.contains("configured_call"));
    Ok(())
}

#[tokio::test]
async fn legacy_json_preserves_failed_invocations_after_source_replacement() -> Result<()> {
    let home = tempfile::tempdir()?;
    let directory = home.path().join("agent-timelines");
    fs::create_dir(&directory)?;
    let path = directory.join("archived.json");
    let original = json!({"version":1,"rows":[
        {"item":{"type":"tool_call","callId":"failed","name":"paseo.list_hosts","status":"failed","detail":{"type":"unknown","input":{}}}},
        {"item":{"type":"tool_call","callId":"external","name":"server.inspect","detail":{"type":"unknown","input":{}}}},
        {"item":{"type":"tool_call","callId":"schema","name":"create_agent","detail":{"type":"plain_text","label":"schema only"}}}
    ]}).to_string();
    fs::write(&path, &original)?;
    let mut catalog = Catalog::default();
    catalog.insert("list_hosts", None);
    catalog.insert("create_agent", None);
    catalog.insert("bridge", Some(("server", "inspect")));
    let first = History::load(home.path(), vec![]).await;
    assert!(first.complete);
    assert!(catalog.essential("list_hosts", &first));
    assert!(catalog.essential("bridge", &first));
    assert!(!catalog.essential("create_agent", &first));
    assert_eq!(fs::read_to_string(&path)?, original);
    fs::write(&path, json!({"version":1,"rows":[
        {"item":{"type":"tool_call","callId":"new","name":"create_agent","detail":{"type":"unknown","input":{}}}}
    ]}).to_string())?;
    let next = History::load(home.path(), vec![]).await;
    assert!(next.complete);
    for name in ["list_hosts", "create_agent", "bridge"] {
        assert!(catalog.essential(name, &next));
    }
    Ok(())
}
