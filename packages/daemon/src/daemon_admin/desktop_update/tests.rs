use super::*;

const ID: &str = "0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11";

fn identities() -> Value {
    let identity = |version: &str, abi| {
        json!({"version":version,"sourceSha":"a".repeat(40),
        "inputHash":"b".repeat(64),"abiVersion":abi})
    };
    json!({"app":identity("1.0.0",1),"daemon":identity("2.0.0",1),
        "tailnet":identity("1.2.0",1),"update-helper":identity("1.5.0",2)})
}
fn record() -> Value {
    use base64::Engine;
    json!({"operationId":ID,"serverId":"host-one","phase":"preparation","helperStarted":true,
        "ownerSession":"private-token","request":{"operationId":ID,
            "expectedRelease":{"version":"2.0.0","manifestSha512":base64::engine::general_purpose::STANDARD.encode([1u8;64])},
            "idleTimeoutMs":300000,"forceRunning":false,"selectedComponents":["daemon"]},
        "targetComponents":identities(),"restartGroup":"daemon",
        "candidate":{"version":"2.0.0","sourceSha":"a".repeat(40),"path":"/private/staged"}})
}
fn helper(phase: &str) -> Value {
    json!({"schemaVersion":2,"operationId":ID,"phase":phase,"expectedComponents":identities(),
        "restartGroup":"daemon","observedComponents":if phase == "committed" {identities()} else {Value::Null},
        "message":"x".repeat(4096),"nonce":"private-nonce"})
}

#[cfg(unix)]
fn helper_request(home: &Path) -> Value {
    let targets = identities()
        .as_object()
        .unwrap()
        .iter()
        .map(|(id, identity)| (id.clone(), json!({"component":{"identity":identity}})))
        .collect::<serde_json::Map<_, _>>();
    json!({"schemaVersion":2,"operationId":ID,"serverId":"host-one","daemonHome":home,
        "restartGroup":"daemon","targetComponents":targets,"manifestSha512":"01".repeat(64)})
}

#[test]
fn projection_requires_exact_helper_identity_and_never_promotes_install_to_commit() {
    let record = record();
    for phase in [
        "preparation",
        "waitingForParent",
        "waitingForAuthorization",
        "installing",
        "starting",
        "checking",
    ] {
        let view = public_operation(&record, Some(&helper(phase)), "host-one", ID).unwrap();
        assert_eq!(view["terminal"], false);
        assert_eq!(view["outcome"], Value::Null);
        assert_eq!(
            view["actionRequired"],
            if phase == "waitingForAuthorization" {
                json!("osAuthorization")
            } else {
                Value::Null
            }
        );
        assert_eq!(view["message"].as_str().unwrap().chars().count(), 1024);
        let serialized = view.to_string();
        for private in [
            "private-token",
            "not-public",
            "/private/staged",
            "private-nonce",
            "ownerSession",
            "request",
        ] {
            assert!(!serialized.contains(private), "{private}");
        }
    }
    for (phase, outcome) in [
        ("committed", "committed"),
        ("failed", "failed"),
        ("rolledBack", "rolled_back"),
    ] {
        let view = public_operation(&record, Some(&helper(phase)), "host-one", ID).unwrap();
        assert_eq!(view["terminal"], true);
        assert_eq!(view["outcome"], outcome);
    }
    let mut unconfirmed = record.clone();
    unconfirmed["phase"] = json!("committed");
    assert!(public_operation(&unconfirmed, None, "host-one", ID).is_err());
    for pointer in [
        "/operationId",
        "/expectedComponents/daemon/version",
        "/observedComponents/app/sourceSha",
        "/restartGroup",
    ] {
        let mut wrong = helper("committed");
        *wrong.pointer_mut(pointer).unwrap() = json!("other");
        assert!(
            public_operation(&record, Some(&wrong), "host-one", ID).is_err(),
            "{pointer}"
        );
    }
    assert!(public_operation(&record, Some(&helper("committed")), "other-host", ID).is_err());
    let mut absent = helper("committed");
    absent["observedComponents"] = Value::Null;
    assert!(public_operation(&record, Some(&absent), "host-one", ID).is_err());
}

fn unplanned_record(phase: &str) -> Value {
    let mut record = record();
    record["candidate"] = Value::Null;
    record["targetComponents"] = json!({});
    record["restartGroup"] = json!("none");
    record["helperStarted"] = json!(false);
    record["phase"] = json!(phase);
    record
}

#[test]
fn unplanned_operations_preserve_approval_without_claiming_activation() {
    for phase in ["admitted", "preparation", "failed"] {
        let record = unplanned_record(phase);
        let view = public_operation(&record, None, "host-one", ID).unwrap();
        assert_eq!(view["targetComponents"], json!({}));
        assert_eq!(view["candidate"], Value::Null);
        assert_eq!(
            view["expectedRelease"],
            record["request"]["expectedRelease"]
        );
        assert_eq!(view["selectedComponents"], json!(["daemon"]));
        assert_eq!(view["terminal"], phase == "failed");
        let mut missing_approval = record.clone();
        missing_approval["request"]["expectedRelease"] = Value::Null;
        assert!(public_operation(&missing_approval, None, "host-one", ID).is_err());
    }
    for phase in [
        "draining",
        "waitingForParent",
        "activating",
        "checking",
        "committed",
        "rolledBack",
    ] {
        assert!(public_operation(&unplanned_record(phase), None, "host-one", ID).is_err());
    }
    for (key, value) in [
        ("helperStarted", json!(true)),
        ("restartGroup", json!("daemon")),
        ("recoveryRequired", json!(true)),
        ("observedComponents", identities()),
        ("targetComponents", identities()),
    ] {
        let mut record = unplanned_record("failed");
        record[key] = value;
        assert!(
            public_operation(&record, None, "host-one", ID).is_err(),
            "{key}"
        );
    }
    let mut status = helper("preparation");
    status["restartGroup"] = json!("none");
    status["expectedComponents"] = json!({});
    assert!(
        public_operation(
            &unplanned_record("preparation"),
            Some(&status),
            "host-one",
            ID
        )
        .is_err()
    );
    for (key, value) in [
        ("targetComponents", json!({})),
        (
            "candidate",
            json!({"version":"other","sourceSha":"a".repeat(40)}),
        ),
        (
            "candidate",
            json!({"version":"2.0.0","sourceSha":"invalid"}),
        ),
    ] {
        let mut planned = record();
        planned[key] = value;
        assert!(public_operation(&planned, None, "host-one", ID).is_err());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn failed_planning_remains_readable_and_releases_operation_admission() {
    let home = private_fixture();
    for phase in ["admitted", "preparation", "failed"] {
        store(home.path(), "operation.json", &unplanned_record(phase)).await;
        let view = status_local(home.path(), "host-one", ID).await.unwrap();
        assert_eq!(view["phase"], phase);
        assert_eq!(
            ensure_no_active(home.path()).await.is_ok(),
            phase == "failed"
        );
    }
}

#[test]
fn retry_identity_includes_release_selection_and_drain_policy() {
    let record = record();
    let request = super::super::remote_submit_request(&record["request"]).unwrap();
    let selected = super::super::selected_components(&record["request"]).unwrap();
    require_request(&record, &request, selected.as_ref()).unwrap();
    for (key, value) in [
        ("selectedComponents", json!(["update-helper"])),
        ("forceRunning", json!(true)),
        ("idleTimeoutMs", json!(1)),
        (
            "expectedRelease",
            json!({"version":"3.0.0","manifestSha512":record["request"]["expectedRelease"]["manifestSha512"]}),
        ),
    ] {
        let mut changed = record.clone();
        changed["request"][key] = value;
        assert!(
            require_request(&changed, &request, selected.as_ref()).is_err(),
            "{key}"
        );
    }
    assert!(require_request(&record, &request, None).is_err());
    for value in [
        json!([]),
        json!(["daemon", "daemon"]),
        json!(["other"]),
        json!("daemon"),
    ] {
        assert!(super::super::selected_components(&json!({"selectedComponents":value})).is_err());
    }
}

#[test]
fn component_only_commit_keeps_app_identity_independent() {
    let record = record();
    let view = public_operation(&record, Some(&helper("committed")), "host-one", ID).unwrap();
    assert_eq!(view["restartGroup"], "daemon");
    assert_eq!(view["observedComponents"]["app"]["version"], "1.0.0");
    assert_eq!(view["observedComponents"]["daemon"]["version"], "2.0.0");
    assert_eq!(view["selectedComponents"], json!(["daemon"]));
    assert_eq!(
        view["expectedRelease"],
        record["request"]["expectedRelease"]
    );
    let mut helper_only = record.clone();
    helper_only["restartGroup"] = json!("none");
    helper_only["request"]["selectedComponents"] = json!(["update-helper"]);
    let mut status = helper("committed");
    status["restartGroup"] = json!("none");
    assert_eq!(
        public_operation(&helper_only, Some(&status), "host-one", ID).unwrap()["restartGroup"],
        "none"
    );
}

#[test]
fn app_policy_reuses_remote_bounds_and_explicit_force() {
    let (force, idle) = drain_policy(&json!({"operationId":ID})).unwrap();
    assert!(!force);
    assert_eq!(
        idle,
        super::super::DEFAULT_UPDATE_IDLE_TIMEOUT.as_millis() as u64
    );
    assert_eq!(
        drain_policy(&json!({"operationId":ID,"forceRunning":true,"idleTimeoutMs":1})).unwrap(),
        (true, 1)
    );
    for invalid in [
        json!({"operationId":"../other"}),
        json!({"operationId":ID,"idleTimeoutMs":0}),
        json!({"operationId":ID,"forceRunning":"yes"}),
        json!({"operationId":ID,"source":"local"}),
    ] {
        assert!(drain_policy(&invalid).is_err());
    }
}

#[cfg(unix)]
fn private_fixture() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let directory = operation_path(home.path(), ID).parent().unwrap().to_owned();
    std::fs::create_dir_all(&directory).unwrap();
    let mut path = directory;
    loop {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        if path == home.path() {
            break;
        }
        path.pop();
    }
    home
}

#[cfg(unix)]
async fn store(home: &Path, name: &str, value: &Value) {
    crate::transport::storage::write_private(
        &operation_path(home, ID).with_file_name(name),
        &serde_json::to_vec(value).unwrap(),
    )
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn disconnected_status_reads_same_operation_and_blocks_standalone_until_terminal() {
    let home = private_fixture();
    store(home.path(), "operation.json", &record()).await;
    store(home.path(), "request.json", &helper_request(home.path())).await;
    store(
        home.path(),
        "status.json",
        &helper("waitingForAuthorization"),
    )
    .await;
    // No owner registry or listening GUI is needed to observe the helper's durable status.
    let pending = status(home.path(), "host-one", ID).await.unwrap();
    assert_eq!(pending["phase"], "waitingForAuthorization");
    assert!(ensure_no_active(home.path()).await.is_err());
    assert!(status_local(home.path(), "host-two", ID).await.is_err());
    assert!(
        status_local(home.path(), "host-one", "../other")
            .await
            .is_err()
    );
    store(home.path(), "status.json", &helper("committed")).await;
    let committed = status_local(home.path(), "host-one", ID).await.unwrap();
    assert_eq!(committed["outcome"], "committed");
    ensure_no_active(home.path()).await.unwrap();
    let mut failed = helper("failed");
    failed["recoveryRequired"] = json!(true);
    store(home.path(), "status.json", &failed).await;
    assert!(ensure_no_active(home.path()).await.is_err());
    assert!(!home.path().join("native-update-operation.json").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn private_status_rejects_symlink_world_readable_and_oversized_records_without_repairing_them()
 {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let home = private_fixture();
    let path = operation_path(home.path(), ID);
    store(home.path(), "operation.json", &record()).await;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(operation(home.path(), "host-one", ID).await.is_err());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
    std::fs::remove_file(&path).unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), serde_json::to_vec(&record()).unwrap()).unwrap();
    symlink(outside.path(), &path).unwrap();
    assert!(operation(home.path(), "host-one", ID).await.is_err());
    std::fs::remove_file(&path).unwrap();
    crate::transport::storage::write_private(&path, &vec![b'x'; MAX_FRAME as usize + 1])
        .await
        .unwrap();
    assert!(operation(home.path(), "host-one", ID).await.is_err());
}

#[test]
fn app_and_native_admission_share_real_exclusive_lease() {
    let home = tempfile::tempdir().unwrap();
    let first = update_operation::AdmissionLease::acquire(home.path(), "admission").unwrap();
    assert!(update_operation::AdmissionLease::acquire(home.path(), "admission").is_err());
    drop(first);
    assert!(update_operation::AdmissionLease::acquire(home.path(), "admission").is_ok());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn owner_registry_binds_live_birth_image_home_server_and_loopback() {
    let home = private_fixture();
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", std::process::id())).unwrap();
    let token = stat[stat.rfind(')').unwrap() + 1..]
        .split_whitespace()
        .nth(19)
        .unwrap();
    let started = chrono::Utc::now().to_rfc3339();
    let daemon = json!({"pid":std::process::id(),"startedAt":started,"hostname":"isolated-fixture",
        "uid":unsafe { libc::geteuid() },"desktopManaged":true,"serverId":"host-one","processStartIdentity":format!("linux:{token}")});
    crate::transport::storage::write_private(
        &home.path().join("paseo.pid"),
        &serde_json::to_vec(&daemon).unwrap(),
    )
    .await
    .unwrap();
    let registry_path = home.path().join("desktop-app-update/owner.json");
    let mut components = identities();
    components["daemon"] = json!(crate::component_identity());
    let registration = json!({"schemaVersion":2,"address":"127.0.0.1:1","token":"a".repeat(64),"serverId":"host-one",
        "daemonHome":home.path(),"daemonPid":std::process::id(),"daemonStartedAt":started,"guiPid":std::process::id(),
        "guiBirthToken":token,"guiExecutable":std::env::current_exe().unwrap().canonicalize().unwrap(),
        "components":components,"capabilities":["desktop-components-v1"]});
    crate::transport::storage::write_private(
        &registry_path,
        &serde_json::to_vec(&registration).unwrap(),
    )
    .await
    .unwrap();
    if validate_identity(&crate::component_identity()).is_ok() {
        owner(home.path(), "host-one").await.unwrap();
    } else {
        // Direct developer builds intentionally lack an authenticated component identity.
        assert!(owner(home.path(), "host-one").await.is_err());
    }
    daemon_control::managed::verify_desktop_process(
        std::process::id(),
        token,
        &std::env::current_exe().unwrap().canonicalize().unwrap(),
    )
    .unwrap();
    for (key, value) in [
        ("daemonHome", json!("/different-home")),
        ("serverId", json!("host-two")),
        ("guiBirthToken", json!("0")),
        ("guiExecutable", json!("/wrong/executable")),
        ("address", json!("192.0.2.1:80")),
        ("daemonStartedAt", json!("other-birth")),
    ] {
        let mut changed = registration.clone();
        changed[key] = value;
        crate::transport::storage::write_private(
            &registry_path,
            &serde_json::to_vec(&changed).unwrap(),
        )
        .await
        .unwrap();
        assert!(owner(home.path(), "host-one").await.is_err(), "{key}");
    }
    assert!(!home.path().join("native-update-operation.json").exists());
    std::fs::remove_file(&registry_path).unwrap();
    let user = tempfile::tempdir().unwrap();
    let app = AppState::new(
        home.path().to_owned(),
        user.path().to_owned(),
        std::env::current_exe().unwrap(),
    );
    let unavailable = super::super::check_update(&app).await;
    assert_eq!(unavailable["targetKind"], "app");
    assert_eq!(unavailable["supported"], false);
    assert!(
        unavailable["supportError"]
            .as_str()
            .unwrap()
            .contains("DESKTOP_OWNER_UNAVAILABLE")
    );
    assert!(!home.path().join("native-update-operation.json").exists());
}

#[tokio::test]
async fn work_drain_times_out_releases_admission_and_force_is_explicit() {
    let home = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    let app = AppState::new(
        home.path().to_owned(),
        user.path().to_owned(),
        std::env::current_exe().unwrap(),
    );
    let active = app.agents.update_admission.enter().unwrap();
    let error = drain_admitted(&app, ID, 10, false).await.unwrap_err();
    assert!(error.to_string().contains("UPDATE_BUSY"));
    assert!(app.agents.update_admission.operation().unwrap().is_none());
    assert!(app.agents.update_admission.enter().is_ok());
    // Force changes only running-work waiting, not policy parsing or host checks.
    assert!(drain_work(&app, 10, true).await.unwrap());
    drop(active);
    assert!(!drain_work(&app, 100, false).await.unwrap());
    app.cancel.cancel();
}

#[tokio::test]
async fn detached_drain_survives_observer_drop_and_only_same_id_can_join() {
    let home = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    let app = AppState::new(
        home.path().to_owned(),
        user.path().to_owned(),
        std::env::current_exe().unwrap(),
    );
    let active = app.agents.update_admission.enter().unwrap();
    let owned = app.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let observer = tokio::spawn(async move {
        let result = drain_admitted(&owned, ID, 500, false).await;
        let _ = done_tx.send(result);
    });
    // Dropping the request's join handle must not cancel its admitted task.
    drop(observer);
    tokio::task::yield_now().await;
    assert_eq!(
        app.agents.update_admission.operation().unwrap().as_deref(),
        Some(ID)
    );
    assert!(pause(&app, ID).is_ok());
    assert!(pause(&app, "184ee51e-01e9-4f76-a6d9-b49265ba5b7e").is_err());
    assert!(app.agents.update_admission.enter().is_err());
    assert!(update_operation::AdmissionLease::acquire(home.path(), "desktop-cutover").is_err());
    drop(active);
    assert_eq!(done_rx.await.unwrap().unwrap()["ready"], true);
    assert_eq!(
        app.agents.update_admission.operation().unwrap().as_deref(),
        Some(ID)
    );
    app.cancel.cancel();
}
