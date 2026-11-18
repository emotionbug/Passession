use super::*;
use serde_json::json;
use tokio::sync::Notify;

fn code(error: anyhow::Error) -> String {
    error.downcast::<ClientError>().unwrap().code
}

async fn write_raw_message(stream: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> io::Result<()> {
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(bytes).await
}

/// The daemon makes its home private (0700); the user's umask must not decide
/// whether the bridge accepts a test home.
fn private_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(
        home.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    home
}

#[tokio::test]
async fn dispatches_only_to_the_owner_of_the_selected_home() {
    let a = private_home();
    let b = private_home();
    let missing = a.path().join("missing");
    let error = request(&missing, Method::Check, &json!({}))
        .await
        .unwrap_err();
    assert_eq!(code(error), "DESKTOP_APP_UNAVAILABLE");
    assert!(Owner::bind(&missing).await.is_err());
    assert!(!missing.exists());
    let owner_a = Owner::bind(a.path()).await.unwrap();
    assert!(Owner::bind(a.path()).await.is_err());
    let owner_b = Owner::bind(b.path()).await.unwrap();
    let server_a = tokio::spawn(owner_a.serve(|method, args| async move {
        match method {
            Method::Check => Ok(json!("a")),
            Method::Submit => Ok(args),
            Method::Status => Err(Refusal::OperationNotFound),
        }
    }));
    let server_b = tokio::spawn(owner_b.serve(|_, _| async { Ok(json!("b")) }));
    assert_eq!(
        request(a.path(), Method::Check, &json!({})).await.unwrap(),
        "a"
    );
    assert_eq!(
        request(b.path(), Method::Check, &json!({})).await.unwrap(),
        "b"
    );
    let submit = json!({"operationId":"id","selectedComponents":["app"],"expectedRelease":{"version":"2","manifestSha512":"pin"}});
    assert_eq!(
        request(a.path(), Method::Submit, &submit).await.unwrap(),
        submit
    );
    assert_eq!(
        code(
            request(a.path(), Method::Status, &json!({"operationId":"id"}))
                .await
                .unwrap_err()
        ),
        "DESKTOP_APP_OPERATION_NOT_FOUND"
    );
    server_a.abort();
    let _ = server_a.await;
    assert_eq!(
        code(
            request(a.path(), Method::Check, &json!({}))
                .await
                .unwrap_err()
        ),
        "DESKTOP_APP_UNAVAILABLE"
    );
    let restarted = Owner::bind(a.path()).await.unwrap();
    let server_a = tokio::spawn(restarted.serve(|_, _| async { Ok(json!("a-after-restart")) }));
    assert_eq!(
        request(a.path(), Method::Check, &json!({})).await.unwrap(),
        "a-after-restart"
    );
    assert_eq!(
        request(b.path(), Method::Check, &json!({})).await.unwrap(),
        "b"
    );
    server_a.abort();
    server_b.abort();
}

#[tokio::test]
async fn discards_malformed_and_overlong_frames_before_dispatch() {
    let home = private_home();
    let owner = Owner::bind(home.path()).await.unwrap();
    let server = tokio::spawn(owner.serve(|_, args| async move { Ok(args) }));
    for frame in [
        br#"{"method":"invoke","args":{}}"#.as_slice(),
        br#"{"method":"check","args":{},"command":"invoke"}"#.as_slice(),
    ] {
        let mut stream = sys::connect(home.path()).await.unwrap();
        write_raw_message(&mut stream, frame).await.unwrap();
        assert!(
            timeout(IO_TIMEOUT, stream.read_u8())
                .await
                .unwrap()
                .is_err()
        );
    }
    let mut stream = sys::connect(home.path()).await.unwrap();
    stream.write_u32(u32::MAX).await.unwrap();
    assert!(
        timeout(IO_TIMEOUT, stream.read_u8())
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(
        code(
            request(
                home.path(),
                Method::Submit,
                &json!({"value":"x".repeat(REQUEST_BYTES)})
            )
            .await
            .unwrap_err()
        ),
        "DESKTOP_APP_UPDATE_FAILED"
    );
    assert_eq!(
        request(home.path(), Method::Check, &json!({}))
            .await
            .unwrap(),
        json!({})
    );
    server.abort();
}

#[tokio::test]
async fn requester_disconnect_does_not_cancel_dispatched_submit() {
    let home = private_home();
    let ready = Arc::new(Notify::new());
    let complete = Arc::new(Notify::new());
    let continue_submit = Arc::new(Notify::new());
    let owner = Owner::bind(home.path()).await.unwrap();
    let server = tokio::spawn(owner.serve({
        let (ready, continue_submit, complete) =
            (ready.clone(), continue_submit.clone(), complete.clone());
        move |method, _| {
            let (ready, continue_submit, complete) =
                (ready.clone(), continue_submit.clone(), complete.clone());
            async move {
                assert_eq!(method, Method::Submit);
                ready.notify_one();
                continue_submit.notified().await;
                complete.notify_one();
                Ok(json!("journaled"))
            }
        }
    }));
    let mut stream = sys::connect(home.path()).await.unwrap();
    write_raw_message(&mut stream, br#"{"method":"submit","args":{}}"#)
        .await
        .unwrap();
    timeout(IO_TIMEOUT, ready.notified()).await.unwrap();
    drop(stream);
    continue_submit.notify_one();
    timeout(IO_TIMEOUT, complete.notified()).await.unwrap();
    server.abort();
}

#[tokio::test]
async fn lost_submit_reply_reports_uncertainty_and_status_uses_original_id() {
    let home = private_home();
    let mut owner = Owner::bind(home.path()).await.unwrap();
    let server = tokio::spawn(async move {
        let mut stream = owner.0.accept().await.unwrap().unwrap();
        let submitted: Request<Value> = read_message(&mut stream, REQUEST_BYTES).await.unwrap();
        assert_eq!(submitted.method, Method::Submit);
        let admitted = json!({"operationId":submitted.args["operationId"],"accepted":true});
        drop(stream);
        owner
            .serve(move |method, args| {
                let admitted = admitted.clone();
                async move {
                    assert_eq!(method, Method::Status);
                    assert_eq!(args["operationId"], admitted["operationId"]);
                    Ok(admitted)
                }
            })
            .await
    });
    let args = json!({"operationId":"unchanged"});
    let error = request(home.path(), Method::Submit, &args)
        .await
        .unwrap_err();
    assert_eq!(code(error), "DESKTOP_APP_OUTCOME_UNKNOWN");
    assert_eq!(
        request(home.path(), Method::Status, &args).await.unwrap(),
        json!({"operationId":"unchanged","accepted":true})
    );
    server.abort();
}

#[cfg(windows)]
#[tokio::test]
async fn rejects_an_unrestricted_named_pipe_before_sending() {
    use std::{mem::size_of, ptr::null_mut};
    use tokio::net::windows::named_pipe::ServerOptions;
    use windows_sys::Win32::{
        Security::{
            InitializeSecurityDescriptor, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
            SetSecurityDescriptorDacl,
        },
        System::SystemServices::SECURITY_DESCRIPTOR_REVISION,
    };

    let home = private_home();
    let name = sys::pipe_name(home.path()).await.unwrap();
    let mut descriptor = SECURITY_DESCRIPTOR::default();
    let descriptor = (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast();
    // SAFETY: the unique test pipe copies this temporary security descriptor.
    let _arbitrary_listener = unsafe {
        assert_ne!(
            InitializeSecurityDescriptor(descriptor, SECURITY_DESCRIPTOR_REVISION),
            0
        );
        assert_ne!(SetSecurityDescriptorDacl(descriptor, 1, null_mut(), 0), 0);
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
            .unwrap()
    };
    let error = request(home.path(), Method::Submit, &json!({}))
        .await
        .unwrap_err();
    assert_eq!(code(error), "DESKTOP_APP_UNAVAILABLE");
}

#[cfg(unix)]
#[tokio::test]
async fn stale_socket_restarts_but_moved_socket_cannot_cross_homes() {
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, net::UnixListener},
    };

    let a = private_home();
    let b = private_home();
    let a_socket = sys::socket_path(a.path()).unwrap();
    let b_socket = sys::socket_path(b.path()).unwrap();
    drop(UnixListener::bind(&a_socket).unwrap());
    fs::set_permissions(&a_socket, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        code(
            request(a.path(), Method::Check, &json!({}))
                .await
                .unwrap_err()
        ),
        "DESKTOP_APP_UNAVAILABLE"
    );
    let owner = Owner::bind(a.path()).await.unwrap();
    let server = tokio::spawn(owner.serve(|_, _| async { Ok(json!("a")) }));
    assert_eq!(
        request(a.path(), Method::Check, &json!({})).await.unwrap(),
        "a"
    );
    fs::rename(&a_socket, &b_socket).unwrap();
    assert_eq!(
        code(
            request(b.path(), Method::Check, &json!({}))
                .await
                .unwrap_err()
        ),
        "DESKTOP_APP_UNAVAILABLE"
    );
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_unsafe_socket_paths_without_removing_them() {
    use std::{fs, os::unix::fs::symlink};

    let home = private_home();
    let socket = sys::socket_path(home.path()).unwrap();
    let other = home.path().join("untouched");
    fs::write(&other, b"unchanged").unwrap();
    symlink(&other, &socket).unwrap();
    assert!(Owner::bind(home.path()).await.is_err());
    assert_eq!(
        code(
            request(home.path(), Method::Check, &json!({}))
                .await
                .unwrap_err()
        ),
        "DESKTOP_APP_UNAVAILABLE"
    );
    assert_eq!(fs::read_link(&socket).unwrap(), other);
    assert_eq!(fs::read(&other).unwrap(), b"unchanged");
}
