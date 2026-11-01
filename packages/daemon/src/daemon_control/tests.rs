use super::*;
use serde_json::json;

#[test]
fn native_lock_excludes_node_compatible_writer_before_any_worker_start() {
    let home = tempfile::tempdir().unwrap();
    let mut lock = lock::PidLock::acquire(home.path(), true).unwrap();
    let serialized: Value =
        serde_json::from_slice(&std::fs::read(home.path().join("paseo.pid")).unwrap()).unwrap();
    assert_eq!(serialized["pid"], std::process::id());
    assert!(serialized["startedAt"].is_string());
    assert_eq!(serialized["heartbeat"], true);
    assert_eq!(serialized["desktopManaged"], true);
    assert!(serialized["processStartIdentity"].is_string());
    assert!(
        !std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(home.path().join("paseo.pid"))
            .is_ok()
    );
    assert!(lock::PidLock::acquire(home.path(), false).is_err());
    lock.publish(Some("127.0.0.1:4242".into()), Some("server".into()))
        .unwrap();
    assert_eq!(
        lock::read(home.path()).unwrap().unwrap().listen.as_deref(),
        Some("127.0.0.1:4242")
    );
    drop(lock);
    assert!(!home.path().join("paseo.pid").exists());
}

#[test]
fn legacy_live_lock_is_reachable_but_never_signal_authority() {
    let home = tempfile::tempdir().unwrap();
    let guard = lock::PidLock::acquire(home.path(), false).unwrap();
    let mut legacy = guard.instance.clone();
    legacy.process_start_identity = None;
    std::fs::write(
        home.path().join("paseo.pid"),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    assert!(identity::owner_running(&legacy).unwrap());
    assert!(!identity::verified(&legacy).unwrap());
    assert!(lock::PidLock::acquire(home.path(), false).is_err());
    drop(guard);
    assert!(home.path().join("paseo.pid").exists());
}

#[test]
fn reused_pid_and_prior_boot_are_not_live_owners() {
    let home = tempfile::tempdir().unwrap();
    let guard = lock::PidLock::acquire(home.path(), false).unwrap();
    let mut stale = guard.instance.clone();
    stale.process_start_identity = Some("not-the-process-birth-token".into());
    assert!(!identity::owner_running(&stale).unwrap());
    assert!(!identity::verified(&stale).unwrap());
    stale = guard.instance.clone();
    stale.started_at = "1970-01-01T00:00:00.000Z".into();
    stale.boot_id = None;
    assert!(!identity::owner_running(&stale).unwrap());
    #[cfg(target_os = "linux")]
    {
        stale = guard.instance.clone();
        stale.boot_id = Some("a-different-boot".into());
        assert!(!identity::owner_running(&stale).unwrap());
    }
}

#[test]
fn release_and_heartbeat_refuse_changed_same_pid_instance() {
    let home = tempfile::tempdir().unwrap();
    let guard = lock::PidLock::acquire(home.path(), false).unwrap();
    let mut replacement = guard.instance.clone();
    replacement.started_at = "2000-01-01T00:00:00.000Z".into();
    std::fs::write(
        home.path().join("paseo.pid"),
        serde_json::to_vec(&replacement).unwrap(),
    )
    .unwrap();
    assert!(guard.refresh().is_err());
    assert!(!lock::release(home.path(), &guard.instance).unwrap());
    drop(guard);
    assert_eq!(
        lock::read(home.path()).unwrap().unwrap().started_at,
        replacement.started_at
    );
}

#[tokio::test]
async fn readiness_is_instance_pinned_cancellable_and_timeout_bounded() {
    let home = tempfile::tempdir().unwrap();
    let mut guard = lock::PidLock::acquire(home.path(), false).unwrap();
    let cancel = CancellationToken::new();
    let failure =
        wait_for_daemon_ready(home.path(), Duration::ZERO, Some(&guard.instance), &cancel)
            .await
            .unwrap_err();
    assert_eq!(
        failure.downcast_ref::<DaemonInstanceError>().unwrap().code,
        "DAEMON_NOT_READY"
    );
    let mut replaced = guard.instance.clone();
    replaced.started_at = "2000-01-01T00:00:00.000Z".into();
    let failure = wait_for_daemon_ready(home.path(), Duration::ZERO, Some(&replaced), &cancel)
        .await
        .unwrap_err();
    assert_eq!(
        failure.downcast_ref::<DaemonInstanceError>().unwrap().code,
        "DAEMON_REPLACED"
    );
    guard
        .publish(
            Some("unix:///isolated/paseo.sock".into()),
            Some("server".into()),
        )
        .unwrap();
    assert!(
        wait_for_daemon_ready(home.path(), Duration::ZERO, Some(&guard.instance), &cancel)
            .await
            .is_ok()
    );
    cancel.cancel();
    assert!(
        wait_for_daemon_ready(home.path(), Duration::ZERO, None, &cancel)
            .await
            .is_err()
    );
}

#[test]
fn invalid_nonempty_lock_is_not_silently_reclaimed() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("paseo.pid"), b"{broken").unwrap();
    assert!(lock::PidLock::acquire(home.path(), false).is_err());
    assert_eq!(
        std::fs::read(home.path().join("paseo.pid")).unwrap(),
        b"{broken"
    );
}

#[cfg(unix)]
#[test]
fn symlinked_pid_file_never_modifies_its_target() {
    use std::os::unix::fs::symlink;
    let home = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let target = other.path().join("target");
    std::fs::write(&target, b"untouched").unwrap();
    symlink(&target, home.path().join("paseo.pid")).unwrap();
    assert!(lock::PidLock::acquire(home.path(), false).is_err());
    assert_eq!(std::fs::read(target).unwrap(), b"untouched");
}

#[test]
fn configured_log_path_is_used_for_startup_diagnostics() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.json"),
        serde_json::to_vec(&json!({"log":{"file":{"path":"logs/worker.log"}}})).unwrap(),
    )
    .unwrap();
    assert_eq!(
        daemon_log_path(home.path()),
        home.path().join("logs/worker.log")
    );
}

#[test]
fn stopping_announcement_carries_intent_and_stays_legacy_compatible() {
    use supervisor::{WorkerExit, stopping_message};
    for (exit, restart) in [
        (WorkerExit::Stop, false),
        (WorkerExit::Restart, true),
        (WorkerExit::Failure, false),
    ] {
        let message = stopping_message("token", exit);
        assert_eq!(message["type"], "paseo:stopping");
        assert_eq!(message["token"], "token");
        // Older supervisors read only `restart`.
        assert_eq!(message["restart"], restart);
        assert_eq!(WorkerExit::from_stopping(&message), exit);
    }
    assert_eq!(
        WorkerExit::from_stopping(&json!({"restart":true})),
        WorkerExit::Restart
    );
    assert_eq!(
        WorkerExit::from_stopping(&json!({"restart":false})),
        WorkerExit::Stop
    );
}

#[test]
fn explicit_worker_stop_ends_supervision_without_recovery() {
    use supervisor::{ExitAction, WorkerExit, exit_action};
    let stop = Some(WorkerExit::Stop);
    assert_eq!(exit_action(false, stop, true, Some(0), false), ExitAction::Exit(0));
    assert_eq!(exit_action(false, stop, true, Some(1), false), ExitAction::Exit(1));
    assert_eq!(exit_action(false, stop, false, None, true), ExitAction::Exit(1));
    // Supervisor-initiated stop wins over any worker announcement.
    for announced in [stop, Some(WorkerExit::Restart), Some(WorkerExit::Failure), None] {
        for ready in [false, true] {
            assert_eq!(
                exit_action(true, announced, ready, Some(1), false),
                ExitAction::Exit(0)
            );
        }
    }
}

#[test]
fn initialization_failure_takes_update_rollback_path() {
    use supervisor::{ExitAction, WorkerExit, exit_action};
    for announced in [Some(WorkerExit::Failure), None] {
        for (code, terminated) in [(Some(1), false), (Some(0), false), (None, true)] {
            assert_eq!(
                exit_action(false, announced, false, code, terminated),
                ExitAction::Recover
            );
        }
    }
    // A requested update restart relaunches without rollback.
    for ready in [false, true] {
        assert_eq!(
            exit_action(false, Some(WorkerExit::Restart), ready, Some(75), false),
            ExitAction::Restart
        );
    }
}

#[test]
fn ready_worker_fatal_error_restarts_instead_of_stopping() {
    use supervisor::{ExitAction, WorkerExit, exit_action};
    let failure = Some(WorkerExit::Failure);
    for (code, terminated) in [(Some(1), false), (None, false), (None, true), (Some(0), false)] {
        assert_eq!(
            exit_action(false, failure, true, code, terminated),
            ExitAction::Restart
        );
    }
    // Unannounced ready exits keep the prior crash policy.
    assert_eq!(exit_action(false, None, true, Some(1), false), ExitAction::Restart);
    assert_eq!(exit_action(false, None, true, Some(75), false), ExitAction::Restart);
    assert_eq!(exit_action(false, None, true, Some(0), false), ExitAction::Exit(0));
    assert_eq!(exit_action(false, None, true, None, true), ExitAction::Exit(1));
}

#[tokio::test]
async fn managed_entries_reject_unknown_arguments_before_any_effect() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().to_string_lossy().into_owned();
    let home = home.as_str();
    for (command, args) in [
        ("__launcher", vec!["--home", home, "--bogus"]),
        ("__update-executor", vec!["--home", home]),
        ("__update-executor", vec!["--home", home, "--operation"]),
        ("__managed-probe", vec!["--home", home]),
        ("__supervisor", vec!["--home", home, "--standalone"]),
    ] {
        let args: Vec<String> = args.into_iter().map(String::from).collect();
        assert!(run_internal(command, &args).await.is_err(), "{command} {args:?}");
    }
    assert!(!directory.path().join("native-installation.json").exists());
    assert!(!directory.path().join("paseo.pid").exists());
    assert_eq!(
        managed::probe_report()["version"],
        env!("CARGO_PKG_VERSION")
    );
}
