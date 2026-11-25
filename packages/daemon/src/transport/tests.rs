use super::*;

fn make_source(id: &str, modern: bool) -> (RequestContext, mpsc::Receiver<context::QueuedFrame>) {
    RequestContext::attach(SourceConfig {
        id: id.into(),
        session_id: "same-session".into(),
        client_id: "same-client".into(),
        capabilities: json!({"owned_subscriptions":modern}),
        principal: json!({"id":"owner","kind":"owner"}),
        permissions: Arc::new(RwLock::new(
            permissions::OWNER_PERMISSIONS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        )),
        routed: false,
        app_version: None,
        cancel: CancellationToken::new(),
    })
}
fn make_request(source: &RequestContext, kind: &str, id: &str) -> RequestContext {
    source.for_request(json!({"type":kind,"requestId":id}))
}
fn json_frame(frame: &context::QueuedFrame) -> Value {
    let OutboundFrame::Text(text) = &frame.frame else {
        panic!("expected text frame")
    };
    serde_json::from_str(text).unwrap()
}

#[test]
fn permission_arrays_are_alternatives_and_unknown_operations_fail_closed() {
    let create = json!({"type":"agent.create.request"});
    assert!(permissions::allows_inbound(&create, |p| p == "hub.execute"));
    assert!(permissions::allows_inbound(&create, |p| p == "workspace.write"));
    assert!(!permissions::allows_inbound(&create, |p| p == "workspace.read"));
    assert!(permissions::allows_inbound(
        &json!({"type":"hosts.route.frame.request"}),
        |_| false
    ));
    assert!(!permissions::allows_inbound(
        &json!({"type":"invented.request"}),
        |_| true
    ));
    assert!(permissions::allows_outbound(
        &json!({"type":"status","payload":{"status":"agent_created"}}),
        |p| p == "hub.execute"
    ));
    assert!(!permissions::allows_outbound(
        &json!({"type":"status","payload":{"status":"daemon_config_changed"}}),
        |p| p == "hub.execute"
    ));
}

#[test]
fn provider_task_stop_requires_workspace_write() {
    let request = json!({"type":"agent.provider_tasks.stop.request"});
    assert!(!permissions::allows_inbound(&request, |p| p == "workspace.read"));
    assert!(!permissions::allows_inbound(&request, |p| p == "hub.execute"));
    assert!(permissions::allows_inbound(&request, |p| p == "workspace.write"));
    assert_eq!(
        permissions::outbound("agent.provider_tasks.stop.response"),
        Some(&["workspace.write"][..])
    );
}

#[test]
fn remote_desktop_app_update_permissions_mirror_daemon_update_permissions() {
    for (operation, required) in [
        ("check", "daemon.read"),
        ("status", "daemon.read"),
        ("submit", "daemon.manage"),
    ] {
        let request = format!("desktop.app.update.{operation}.request");
        let response = format!("desktop.app.update.{operation}.response");
        assert_eq!(permissions::inbound(&request), Some(&[required][..]));
        assert_eq!(permissions::outbound(&response), Some(&[required][..]));
        assert_eq!(
            permissions::inbound(&request),
            permissions::inbound(&format!("daemon.update.{operation}.request"))
        );
        assert_eq!(
            permissions::outbound(&response),
            permissions::outbound(&format!("daemon.update.{operation}.response"))
        );
    }
}

#[tokio::test]
async fn remote_desktop_app_update_cannot_bypass_direct_or_routed_grants() {
    let root = tempfile::tempdir().unwrap();
    let app = AppState::new(
        root.path().join("home"),
        root.path().into(),
        std::env::current_exe().unwrap(),
    );
    app.config.initialize().await.unwrap();
    let all = ["check", "status", "submit"];
    let submit_only = ["submit"];
    // Granted link permissions, forwarded session ceiling, denied operations.
    type Case<'a> = (&'a [&'a str], Option<&'a [&'a str]>, &'a [&'a str]);
    let cases: [Case<'_>; 4] = [
        (&["workspace.read"], None, &all),
        (
            &["workspace.read", "daemon.manage"],
            Some(&["workspace.read", "daemon.read"]),
            &all,
        ),
        (
            &["daemon.read", "daemon.manage"],
            Some(&["daemon.read"]),
            &submit_only,
        ),
        (
            &["daemon.read"],
            Some(&["daemon.read", "daemon.manage"]),
            &submit_only,
        ),
    ];
    for (index, (link_grants, forwarded, denied)) in cases.into_iter().enumerate() {
        let (input, inbound) = mpsc::channel(4);
        let (outbound, mut output) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let mut hello = json!({
            "type":"hello", "clientId":format!("app-update-permission-{index}"), "clientType":"cli",
            "protocolVersion":1, "capabilities":{"owned_subscriptions":true}
        });
        if let Some(forwarded) = forwarded {
            hello["routed"] = json!(true);
            hello["producer"] = json!({"serverId":"origin","sourceId":"caller"});
            hello["sessionPermissions"] = json!(forwarded);
        }
        input
            .send(OutboundFrame::Text(hello.to_string()))
            .await
            .unwrap();
        for &operation in denied {
            let mut request = json!({
                "type":format!("desktop.app.update.{operation}.request"), "requestId":operation
            });
            if operation != "check" {
                request["operationId"] = json!("0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11");
            }
            if operation == "submit" {
                request["selectedComponents"] = json!(["app"]);
                request["expectedRelease"] =
                    json!({"version":"2.0.0","manifestSha512":format!("{}==", "A".repeat(86))});
            }
            input
                .send(OutboundFrame::Text(
                    json!({"type":"session","message":request}).to_string(),
                ))
                .await
                .unwrap();
        }
        let admission = Admission {
            permissions: link_grants
                .iter()
                .map(|grant| (*grant).to_owned())
                .collect(),
            ..Admission::owner()
        };
        let server_app = app.clone();
        let server_cancel = cancel.clone();
        let server = tokio::spawn(async move {
            server_app
                .transport
                .run_channel(inbound, outbound, server_cancel, Some(admission))
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            let Some(OutboundFrame::Text(text)) = output.recv().await else {
                panic!("no server handshake");
            };
            let info: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(info["message"]["payload"]["status"], "server_info");
            for &operation in denied {
                let Some(OutboundFrame::Text(text)) = output.recv().await else {
                    panic!("no permission error for {operation}");
                };
                let reply: Value = serde_json::from_str(&text).unwrap();
                let message = &reply["message"];
                assert_eq!(message["type"], "rpc_error");
                assert_eq!(message["payload"]["requestId"], operation);
                assert_eq!(
                    message["payload"]["requestType"],
                    format!("desktop.app.update.{operation}.request")
                );
                assert_eq!(message["payload"]["code"], "access_denied");
            }
        })
        .await
        .expect("permission responses timed out");
        cancel.cancel();
        server.await.unwrap().unwrap();
    }
}

fn schema_message_types(schema: &str) -> Vec<String> {
    let schema: Value = serde_json::from_str(schema).unwrap();
    schema["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|variant| {
            variant["properties"]["type"]["const"]
                .as_str()
                .expect("every wire variant names its type")
                .to_owned()
        })
        .collect()
}

/// A message the protocol declares but the permission table omits fails closed at send time,
/// which silently broke checkout observation; every declared session message needs an entry.
#[test]
fn every_declared_session_message_has_a_permission_entry() {
    let missing: Vec<_> = schema_message_types(include_str!(
        "../../../protocol/schema/session-outbound.json"
    ))
    .into_iter()
    .filter(|kind| permissions::outbound(kind).is_none())
    .collect();
    assert!(
        missing.is_empty(),
        "outbound without permission: {missing:?}"
    );
    let missing: Vec<_> = schema_message_types(include_str!(
        "../../../protocol/schema/session-inbound.json"
    ))
    .into_iter()
    .filter(|kind| permissions::inbound(kind).is_none())
    .collect();
    assert!(
        missing.is_empty(),
        "inbound without permission: {missing:?}"
    );
}

#[tokio::test]
async fn replies_require_request_type_id_and_original_live_source() {
    let (old, mut old_output) = make_source("old", true);
    let (replacement, mut replacement_output) = make_source("replacement", true);
    let request = make_request(&old, "fetch_agent_request", "r");
    assert!(
        request
            .send(json!({"type":"fetch_agents_response","payload":{"requestId":"r"}}))
            .await
            .is_err()
    );
    assert!(
        request
            .send(json!({"type":"fetch_agent_response","payload":{"requestId":"other"}}))
            .await
            .is_err()
    );
    request
        .send(json!({"type":"fetch_agent_response","payload":{"requestId":"r","agent":null}}))
        .await
        .unwrap();
    assert_eq!(
        json_frame(&old_output.recv().await.unwrap())["message"]["payload"]["agent"],
        Value::Null
    );
    old.detach();
    assert!(
        request
            .send(json!({"type":"fetch_agent_response","payload":{"requestId":"r"}}))
            .await
            .is_err()
    );
    assert!(replacement_output.try_recv().is_err());
    assert!(replacement.is_live());
}

#[tokio::test]
async fn released_request_cannot_emit_without_retained_operation() {
    let (source, mut output) = make_source("physical", true);
    let request = make_request(&source, "usage.list_reports.request", "usage");
    let operation = request
        .retain_operation(&["usage.list_reports.update"])
        .unwrap();
    request.finish_request();
    assert!(
        request
            .send(json!({"type":"usage.list_reports.update","payload":{"requestId":"usage"}}))
            .await
            .is_err()
    );
    operation
        .emit(json!({"type":"usage.list_reports.update","payload":{"requestId":"usage"}}))
        .await
        .unwrap();
    assert_eq!(
        json_frame(&output.recv().await.unwrap())["message"]["type"],
        "usage.list_reports.update"
    );
    operation.release().await.unwrap();
    assert!(
        operation
            .emit(json!({"type":"usage.list_reports.update","payload":{}}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn snapshot_precedes_buffered_updates_and_only_owner_releases() {
    let (source, mut output) = make_source("one", true);
    let (other, _) = make_source("two", true);
    let request = make_request(&source, "fetch_agents_request", "snapshot");
    let subscription = request.begin_subscription("agents", None, None).unwrap();
    subscription
        .emit(json!({"type":"agent_update","payload":{"agent":{"id":"changed"}}}))
        .await
        .unwrap();
    assert!(output.try_recv().is_err());
    request.send(json!({"type":"fetch_agents_response","payload":{"requestId":"snapshot","subscriptionId":subscription.response_id(),"agents":[]}})).await.unwrap();
    subscription.activate().await.unwrap();
    assert_eq!(
        json_frame(&output.recv().await.unwrap())["message"]["type"],
        "fetch_agents_response"
    );
    let update = json_frame(&output.recv().await.unwrap());
    assert_eq!(update["message"]["type"], "agent_update");
    assert_eq!(
        update["message"]["payload"]["subscriptionId"],
        subscription.id()
    );
    make_request(&other, "subscription.release.request", "release")
        .release_subscription(subscription.id())
        .await
        .unwrap();
    assert!(!subscription.cancellation().is_cancelled());
    make_request(&source, "subscription.release.request", "release")
        .release_subscription(subscription.id())
        .await
        .unwrap();
    assert!(subscription.cancellation().is_cancelled());
}

#[tokio::test]
async fn terminal_slots_are_physical_source_and_subscription_owned() {
    let (first, mut output) = make_source("first", true);
    let (second, _) = make_source("second", true);
    let subscription = make_request(&first, "subscribe_terminal_request", "sub")
        .begin_subscription("terminal", None, None)
        .unwrap();
    subscription.bind_terminal_slot(7).unwrap();
    subscription.activate().await.unwrap();
    assert!(first.validate_binary(&[2, 7, b'x'], true).is_ok());
    assert!(second.validate_binary(&[2, 7, b'x'], true).is_err());
    assert!(
        first
            .send_binary(Bytes::from_static(&[1, 7, b'x']))
            .await
            .is_err()
    );
    subscription
        .emit_binary(Bytes::from_static(&[1, 7, b'x']))
        .await
        .unwrap();
    assert!(matches!(
        &output.recv().await.unwrap().frame,
        OutboundFrame::Binary(_)
    ));
    subscription.release().await.unwrap();
    assert!(first.validate_binary(&[2, 7, b'x'], true).is_err());
}

#[tokio::test]
async fn legacy_slot_replacement_reclaims_binary_binding_synchronously() {
    let (source, _) = make_source("legacy", false);
    let request = make_request(&source, "subscribe_terminal_request", "r");
    let first = request
        .begin_subscription("terminals", Some("client-id"), Some("terminal-A"))
        .unwrap();
    first.bind_terminal_slot(5).unwrap();
    let second = request
        .begin_subscription("terminals", Some("client-next"), Some("terminal-A"))
        .unwrap();
    second.bind_terminal_slot(5).unwrap();
    assert!(first.cancellation().is_cancelled());
    assert_eq!(second.response_id(), "client-next");
}

#[tokio::test]
async fn modern_subscription_ids_are_host_assigned_and_snapshot_budget_is_shared() {
    let (source, _) = make_source("modern", true);
    let request = make_request(&source, "subscribe_terminal_request", "r");
    assert!(
        request
            .begin_subscription("terminal", Some("caller-id"), None)
            .is_err()
    );
    let first = request.begin_subscription("terminal", None, None).unwrap();
    first.bind_terminal_slot(1).unwrap();
    let second = request.begin_subscription("terminal", None, None).unwrap();
    second.bind_terminal_slot(2).unwrap();
    let mut frame = vec![0u8; MAX_BUFFERED_BYTES / 2 + 1];
    frame[0] = 1;
    frame[1] = 1;
    first.emit_binary(Bytes::from(frame)).await.unwrap();
    let mut frame = vec![0u8; MAX_BUFFERED_BYTES / 2 + 1];
    frame[0] = 1;
    frame[1] = 2;
    assert!(second.emit_binary(Bytes::from(frame)).await.is_err());
    assert!(!source.is_live());
}

#[tokio::test(start_paused = true)]
async fn a_download_larger_than_the_buffer_waits_for_a_slow_link_instead_of_disconnecting() {
    let (source, mut output) = make_source("slow-link", true);
    let request = make_request(&source, "file_explorer_request", "download");
    let mut frame = vec![0x11, 8];
    frame.extend_from_slice(b"download");
    frame.resize(frame.len() + 256 * 1024, 7);
    let chunk = Bytes::from(frame);
    let frames = MAX_BUFFERED_BYTES / chunk.len() + 64;
    let producer = tokio::spawn(async move {
        for _ in 0..frames {
            request.send_binary(chunk.clone()).await?;
        }
        anyhow::Ok(())
    });
    // A slow socket: the writer keeps each batch reserved until the link has taken all of it.
    let (mut batch, mut received, mut peak) = (Vec::new(), 0, 0);
    while received < frames && !(producer.is_finished() && output.is_empty()) {
        match tokio::time::timeout(Duration::from_millis(50), output.recv()).await {
            Ok(Some(frame)) => {
                batch.push(frame);
                received += 1;
                peak = peak.max(source.buffered_amount());
            }
            Ok(None) => break,
            Err(_) => batch.clear(),
        }
    }
    producer.await.unwrap().unwrap();
    assert_eq!(received, frames);
    assert!(source.is_live());
    assert!(peak < MAX_BUFFERED_BYTES / 4, "{peak} bytes queued");
}

#[test]
fn file_upload_frames_require_source_registration_and_exact_end_length() {
    let (source, _) = make_source("a", true);
    let (other, _) = make_source("b", true);
    let req = make_request(&source, "file.upload.request", "upload");
    req.bind_file_transfer("u").unwrap();
    assert!(source.validate_binary(&[0x11, 1, b'u', 9], true).is_ok());
    assert!(other.validate_binary(&[0x11, 1, b'u', 9], true).is_err());
    assert!(source.validate_binary(&[0x12, 1, b'u', 9], true).is_err());
    assert!(source.validate_binary(&[0x12, 1, b'u'], true).is_ok());
    req.release_file_transfer("u");
    assert!(source.validate_binary(&[0x11, 1, b'u', 9], true).is_err());
}

#[tokio::test(start_paused = true)]
async fn rejected_upload_frame_grace_is_bounded_and_source_local() {
    let (source, _) = make_source("a", true);
    let (other, _) = make_source("b", true);
    source.drain_file_transfer("first");
    let first = b"\x12\x05first";
    assert!(source.validate_binary(first, true).is_ok());
    assert!(other.validate_binary(first, true).is_err());
    for i in 0..40 {
        source.drain_file_transfer(&format!("rejected-{i}"));
    }
    assert!(source.validate_binary(first, true).is_err());
    let most_recent = b"\x12\x0brejected-39";
    assert!(source.validate_binary(most_recent, true).is_ok());
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(source.validate_binary(most_recent, true).is_err());
    assert!(source.is_live());
}

#[tokio::test]
async fn notification_subscription_deduplicates_per_physical_source() {
    let (source, mut output) = make_source("a", true);
    for id in ["one", "two"] {
        let request = make_request(&source, "session.events.set_subscription.request", id);
        let subscription = request.begin_subscription("events", None, None).unwrap();
        request.register_events(
            &subscription,
            ["agent_attention_required".to_owned()]
                .into_iter()
                .collect(),
            true,
        );
        subscription.activate().await.unwrap();
    }
    source
        .emit_event(json!({"type":"agent_attention_required","payload":{"shouldNotify":true}}))
        .await
        .unwrap();
    let first = json_frame(&output.recv().await.unwrap());
    let second = json_frame(&output.recv().await.unwrap());
    assert_eq!(
        [
            first["message"]["payload"]["shouldNotify"]
                .as_bool()
                .unwrap(),
            second["message"]["payload"]["shouldNotify"]
                .as_bool()
                .unwrap()
        ]
        .into_iter()
        .filter(|v| *v)
        .count(),
        1
    );
}

#[test]
fn host_binary_preserves_payload_and_rejects_bad_route_utf8() {
    let payload = [0x10, 2, b'a', b'b', 0, 0];
    let frame = encode_host_frame("route-δ", &payload).unwrap();
    let (route, body) = decode_host_frame(&frame).unwrap();
    assert_eq!(route, "route-δ");
    assert_eq!(body, payload);
    assert!(decode_host_frame(&[0x30, 0, 1, 0xff, 0]).is_err());
    assert!(encode_host_frame("", &[]).is_err());
}

#[tokio::test]
async fn web_ui_serves_assets_after_a_live_config_patch_without_changing_daemon_routes() {
    use ::http::{Method, Request, StatusCode};
    use http_body_util::{BodyExt, Empty};
    use hyper::client::conn::http1;
    use hyper_util::rt::TokioIo;

    let root = tempfile::tempdir().unwrap();
    let web = root.path().join("web");
    let home = root.path().join("home");
    std::fs::create_dir_all(web.join("api")).unwrap();
    std::fs::write(web.join("index.html"), b"<title>Passession</title>").unwrap();
    std::fs::write(web.join("app.wasm"), b"\0asm").unwrap();
    std::fs::write(web.join("ws"), b"not a websocket").unwrap();
    std::fs::write(web.join("api/health"), b"not a health response").unwrap();
    std::fs::write(web.join("api/status"), b"not a status response").unwrap();
    std::fs::write(root.path().join("outside.txt"), b"outside the bundle").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.path().join("outside.txt"), web.join("escape.txt")).unwrap();
    let app = AppState::new(home, root.path().into(), root.path().join("paseo"));
    app.config
        .initialize_with_overrides(&json!({"listen": "127.0.0.1:0", "relayEnabled": false}))
        .await
        .unwrap();
    app.transport.initialize().await.unwrap();
    let authority = app.transport.listen_string().unwrap();
    let stream = tokio::net::TcpStream::connect(&authority).await.unwrap();
    let (mut sender, conn) = http1::handshake(TokioIo::new(stream)).await.unwrap();
    let conn = tokio::spawn(conn);

    let request = |method: Method, path: &str| {
        Request::builder()
            .method(method)
            .uri(path)
            .header("host", authority.as_str())
            .body(Empty::<Bytes>::new())
            .unwrap()
    };
    let response = sender
        .send_request(request(Method::GET, "/"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        Bytes::from_static(b"404 Not Found")
    );

    // With a password set, only the bundle bypasses bearer authentication.
    app.transport.config.write().unwrap()["auth"] =
        json!({"password": bcrypt::hash("web test password", 4).unwrap()});
    let config = app
        .config
        .patch(json!({"web": {"enabled": true}}))
        .await
        .unwrap();
    assert_eq!(config["web"]["enabled"], true);
    for (path, mime, body) in [
        (
            "/",
            "text/html; charset=utf-8",
            b"<title>Passession</title>".as_slice(),
        ),
        ("/app.wasm", "application/wasm", b"\0asm".as_slice()),
    ] {
        let response = sender
            .send_request(request(Method::GET, path))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], mime);
        assert_eq!(response.headers()["cache-control"], "no-cache");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            body
        );
    }
    let response = sender
        .send_request(request(Method::HEAD, "/app.wasm"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-length"], "4");
    assert!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );

    // Revalidation answers 304; a gzip client gets the precompressed sibling.
    let response = sender
        .send_request(request(Method::GET, "/app.wasm"))
        .await
        .unwrap();
    let tag = response.headers()["etag"].clone();
    response.into_body().collect().await.unwrap();
    let mut conditional = request(Method::GET, "/app.wasm");
    conditional.headers_mut().insert("if-none-match", tag);
    let response = sender.send_request(conditional).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    std::fs::write(web.join("app.wasm.gz"), b"gzipped").unwrap();
    for (accept, encoding, body) in [
        ("br, gzip", Some("gzip"), b"gzipped".as_slice()),
        ("gzip;q=0", None, b"\0asm".as_slice()),
    ] {
        let mut gzip = request(Method::GET, "/app.wasm");
        gzip.headers_mut()
            .insert("accept-encoding", accept.parse().unwrap());
        let response = sender.send_request(gzip).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{accept}");
        assert_eq!(response.headers()["content-type"], "application/wasm");
        assert_eq!(
            response
                .headers()
                .get("content-encoding")
                .map(|value| value.to_str().unwrap()),
            encoding,
            "{accept}"
        );
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            body,
            "{accept}"
        );
    }
    // `cargo xtask build-web` stores large assets only gzipped; a client that refuses
    // gzip still gets the decoded file.
    let script = b"console.log('boot');\n".repeat(64);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    std::io::Write::write_all(&mut encoder, &script).unwrap();
    let compressed = encoder.finish().unwrap();
    std::fs::write(web.join("boot.js.gz"), &compressed).unwrap();
    for (accept, encoding, body) in [
        ("gzip", Some("gzip"), compressed.as_slice()),
        ("identity", None, script.as_slice()),
    ] {
        let mut asset = request(Method::GET, "/boot.js");
        asset
            .headers_mut()
            .insert("accept-encoding", accept.parse().unwrap());
        let response = sender.send_request(asset).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{accept}");
        assert_eq!(
            response.headers()["content-type"],
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            response
                .headers()
                .get("content-encoding")
                .map(|value| value.to_str().unwrap()),
            encoding,
            "{accept}"
        );
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            body,
            "{accept}"
        );
    }

    for (path, status) in [
        ("/missing", StatusCode::NOT_FOUND),
        ("/%2e%2e/outside.txt", StatusCode::NOT_FOUND),
        ("/%2e%2e%2foutside.txt", StatusCode::NOT_FOUND),
        #[cfg(unix)]
        ("/escape.txt", StatusCode::NOT_FOUND),
        ("/ws", StatusCode::BAD_REQUEST),
        ("/api/status", StatusCode::UNAUTHORIZED),
        ("/api/health", StatusCode::OK),
    ] {
        let response = sender
            .send_request(request(Method::GET, path))
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        if path == "/api/health" {
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap()["status"],
                "ok"
            );
        }
    }
    let mut disallowed = request(Method::GET, "/");
    disallowed
        .headers_mut()
        .insert("host", "not-allowed.example".parse().unwrap());
    let response = sender.send_request(disallowed).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    response.into_body().collect().await.unwrap();

    std::fs::remove_file(web.join("index.html")).unwrap();
    let response = sender
        .send_request(request(Method::GET, "/"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let explanation = response.into_body().collect().await.unwrap().to_bytes();
    assert!(
        std::str::from_utf8(&explanation)
            .unwrap()
            .contains("PASEO_WEB_DIR")
    );

    let config = app
        .config
        .patch(json!({"web": {"enabled": false}}))
        .await
        .unwrap();
    assert_eq!(config["web"]["enabled"], false);
    assert_eq!(
        sender
            .send_request(request(Method::GET, "/"))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    drop(sender);
    app.transport.shutdown().await.unwrap();
    conn.await.unwrap().unwrap();
}

#[tokio::test]
async fn loopback_websocket_rejects_wrong_protocol_and_reserved_plugin_identity() {
    for (client_id, version, code, reason) in [
        ("regular", 0, 4003, "incompatible_protocol"),
        // Original order: protocol rejection precedes the trimmed-clientId check.
        ("   ", 0, 4003, "incompatible_protocol"),
        ("   ", 1, 4002, ""),
        ("plugin:spoof", 1, 4002, ""),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let transport = Arc::new(TransportService::new(Weak::new()));
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            transport
                .run_socket(socket, None, CancellationToken::new())
                .await
                .unwrap();
        });
        let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
            .await
            .unwrap();
        client.send(Message::Text(json!({"type":"hello","clientId":client_id,"clientType":"cli","protocolVersion":version,"capabilities":{"hello_rejection":true}}).to_string().into())).await.unwrap();
        if !reason.is_empty() {
            let message = client.next().await.unwrap().unwrap();
            let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            assert_eq!(value["type"], "hello.rejected");
            assert_eq!(value["reason"], reason);
        }
        let Message::Close(Some(close)) = client.next().await.unwrap().unwrap() else {
            panic!("expected close")
        };
        assert_eq!(u16::from(close.code), code);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn upload_registration_drains_rejected_frames_without_blocking_other_messages() {
    use paseo_protocol::binary_frames::{
        FileBeginMetadata, FileEncoding, FileTransferFrame, encode_file_transfer_frame,
    };

    let root = tempfile::tempdir().unwrap();
    let app = AppState::new(
        root.path().join("home"),
        root.path().into(),
        std::env::current_exe().unwrap(),
    );
    app.config.initialize().await.unwrap();
    app.files.initialize().await.unwrap();
    let (input, inbound) = mpsc::channel(16);
    let (outbound, mut output) = mpsc::channel(8);
    let cancel = CancellationToken::new();
    let payload = "한글 첨부 파일\n".as_bytes();
    // Queue the complete transfer before running the reader. Uploads have no
    // intermediate acknowledgement, so these frames must remain ordered.
    input
        .send(OutboundFrame::Text(
            json!({
                "type":"hello", "clientId":"upload-test", "clientType":"cli",
                "protocolVersion":1, "capabilities":{"owned_subscriptions":true}
            })
            .to_string(),
        ))
        .await
        .unwrap();
    input
        .send(OutboundFrame::Text(
            json!({"type":"session","message":{"type":"file.upload.request",
                "requestId":"too-large","fileName":"oversized.txt","size":51 * 1024 * 1024,
                "mimeType":"text/plain","modifiedAt":""}})
            .to_string(),
        ))
        .await
        .unwrap();
    for frame in [
        FileTransferFrame::Begin {
            request_id: "too-large",
            metadata: FileBeginMetadata {
                mime: "text/plain".into(),
                size: 51 * 1024 * 1024,
                encoding: FileEncoding::Binary,
                modified_at: String::new(),
                revision: None,
                file_name: None,
            },
        },
        FileTransferFrame::Chunk {
            request_id: "too-large",
            payload: b"already sent",
        },
        FileTransferFrame::End {
            request_id: "too-large",
        },
    ] {
        input
            .send(OutboundFrame::Binary(
                encode_file_transfer_frame(&frame).unwrap().into(),
            ))
            .await
            .unwrap();
    }
    input
        .send(OutboundFrame::Text(r#"{"type":"ping"}"#.into()))
        .await
        .unwrap();
    input
        .send(OutboundFrame::Text(
            json!({
                "type":"session", "message":{
                    "type":"file.upload.request", "requestId":"upload", "fileName":"한글.txt",
                    "mimeType":"text/plain", "size":payload.len(), "modifiedAt":""
                }
            })
            .to_string(),
        ))
        .await
        .unwrap();
    for frame in [
        FileTransferFrame::Begin {
            request_id: "upload",
            metadata: FileBeginMetadata {
                mime: "text/plain".into(),
                size: payload.len() as u64,
                encoding: FileEncoding::Binary,
                modified_at: String::new(),
                revision: None,
                file_name: Some("한글.txt".into()),
            },
        },
        FileTransferFrame::Chunk {
            request_id: "upload",
            payload,
        },
        FileTransferFrame::End {
            request_id: "upload",
        },
    ] {
        input
            .send(OutboundFrame::Binary(
                encode_file_transfer_frame(&frame).unwrap().into(),
            ))
            .await
            .unwrap();
    }
    let server_app = app.clone();
    let server_cancel = cancel.clone();
    let server = tokio::spawn(async move {
        server_app
            .transport
            .run_channel(inbound, outbound, server_cancel, Some(Admission::owner()))
            .await
    });
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        let mut rejected = false;
        let mut pong = false;
        loop {
            match output
                .recv()
                .await
                .expect("upload channel closed before its response")
            {
                OutboundFrame::Text(text) => {
                    let envelope: Value = serde_json::from_str(&text).unwrap();
                    if envelope["type"] == "pong" {
                        pong = true;
                    }
                    if envelope["message"]["type"] == "rpc_error" {
                        let error = &envelope["message"]["payload"];
                        assert_eq!(error["requestId"], "too-large");
                        assert_eq!(error["code"], "FILE_TOO_LARGE");
                        assert_eq!(error["error"], "oversized.txt");
                        rejected = true;
                    }
                    if envelope["message"]["type"] == "file.upload.response" {
                        assert!(rejected);
                        assert!(pong);
                        break envelope["message"]["payload"].clone();
                    }
                }
                OutboundFrame::Flush(acknowledge) => {
                    acknowledge.send(Ok(())).unwrap();
                }
                OutboundFrame::Close { code, reason } => panic!("upload closed: {code} {reason}"),
                _ => {}
            }
        }
    })
    .await
    .expect("upload response timed out");
    assert_eq!(response["error"], Value::Null);
    assert_eq!(response["file"]["fileName"], "한글.txt");
    assert_eq!(response["file"]["size"], payload.len());
    assert_eq!(
        tokio::fs::read(response["file"]["path"].as_str().unwrap())
            .await
            .unwrap(),
        payload
    );
    cancel.cancel();
    server.await.unwrap().unwrap();
    app.files.shutdown().await.unwrap();
}

#[test]
fn hello_validates_grant_vocabulary_without_rejecting_newer_protocol_versions() {
    let mut hello = json!({"type":"hello","clientId":"client","clientType":"cli","protocolVersion":2,"sessionPermissions":["workspace.read"]});
    assert!(validate_hello(&hello).is_ok());
    hello["sessionPermissions"] = json!(["unknown.manage"]);
    assert!(validate_hello(&hello).is_err());
    hello["sessionPermissions"] = json!([]);
    hello["clientId"] = json!("plugin:allowed");
    assert!(validate_hello(&hello).is_ok());
    assert!(validate_hello_identity(&hello, Some("allowed")).is_ok());
    assert!(validate_hello_identity(&hello, None).is_err());
    hello["clientId"] = json!("  ");
    assert!(validate_hello(&hello).is_ok());
    assert!(validate_hello_identity(&hello, None).is_err());
    hello["clientId"] = json!("");
    assert!(validate_hello(&hello).is_err());
    hello["clientId"] = json!("regular");
    hello["protocolVersion"] = json!(1.0);
    assert!(validate_hello(&hello).is_ok());
    hello["protocolVersion"] = json!(1.5);
    assert!(validate_hello(&hello).is_err());
}

#[tokio::test]
async fn disabled_pairing_does_not_initialize_identity_or_credentials() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("not-created");
    let offer = generate_local_pairing_offer(&home, &json!({"relayEnabled":false}), true)
        .await
        .unwrap();
    assert_eq!(offer, json!({"relayEnabled":false,"url":null,"qr":null}));
    assert!(!home.exists());
}

#[tokio::test]
async fn pairing_reuses_persisted_identity_and_stamps_protected_floor() {
    let home = tempfile::tempdir().unwrap();
    let config = json!({"relayEnabled":true,"relayPublicEndpoint":"relay.example:443","relayPublicUseTls":true,"relayRequireProtectedSecurity":true,"appBaseUrl":"https://native.example/"});
    let first = generate_local_pairing_offer(home.path(), &config, false)
        .await
        .unwrap();
    let second = generate_local_pairing_offer(home.path(), &config, false)
        .await
        .unwrap();
    assert_eq!(first, second);
    assert!(first["qr"].is_null());
    let encoded = first["url"]
        .as_str()
        .unwrap()
        .split("#offer=")
        .nth(1)
        .unwrap();
    let offer: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap();
    assert_eq!(offer["v"], 2);
    assert_eq!(offer["minimumSecurityMode"], "protected");
    let key: Value = serde_json::from_slice(
        &tokio::fs::read(home.path().join("daemon-keypair.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(offer["daemonPublicKeyB64"], key["publicKeyB64"]);
    assert!(!home.path().join("local-credential").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(home.path().join("daemon-keypair.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn malformed_existing_identity_never_silently_changes_pairing_keys() {
    let home = tempfile::tempdir().unwrap();
    storage::write_private(&home.path().join("server-id"), b"srv_existing\n")
        .await
        .unwrap();
    let corrupt = b"{\"v\":2,\"publicKeyB64\":\"invalid\",\"secretKeyB64\":\"invalid\"}";
    storage::write_private(&home.path().join("daemon-keypair.json"), corrupt)
        .await
        .unwrap();
    assert!(
        generate_local_pairing_offer(home.path(), &json!({"relayEnabled":true}), false)
            .await
            .is_err()
    );
    assert_eq!(
        tokio::fs::read(home.path().join("daemon-keypair.json"))
            .await
            .unwrap(),
        corrupt
    );
}

#[tokio::test]
async fn hub_live_grant_updates_cannot_exceed_hello_permission_ceiling() {
    let transport = TransportService::new(Weak::new());
    let (context, _) = make_source("source", true);
    let key = serde_json::to_string(&("hub:relationship", "client")).unwrap();
    transport.sessions.lock().unwrap().sessions.insert(
        key,
        Session {
            id: "session".into(),
            permissions: context.source.permissions.clone(),
            sources: [(context.source_id().into(), Arc::downgrade(&context.source))]
                .into_iter()
                .collect(),
            retained_until: None,
            ephemeral: false,
            permission_ceiling: Some(["daemon.read".to_owned()].into_iter().collect()),
        },
    );
    transport
        .update_hub_permissions(
            "hub:relationship",
            &[
                "daemon.read".into(),
                "workspace.write".into(),
                "hub.execute".into(),
            ],
        )
        .await
        .unwrap();
    assert!(context.allows("daemon.read"));
    assert!(!context.allows("workspace.write"));
    assert!(!context.allows("hub.execute"));
}

#[tokio::test]
async fn flushed_reply_waits_for_writer_acknowledgement_and_fails_on_detach() {
    let (source, mut output) = make_source("source", true);
    let request = make_request(&source, "shutdown_server_request", "stop");
    let send = tokio::spawn(async move {
        request.send_flushed(json!({"type":"status","payload":{"status":"shutdown_requested","requestId":"stop"}})).await
    });
    assert_eq!(
        json_frame(&output.recv().await.unwrap())["message"]["payload"]["status"],
        "shutdown_requested"
    );
    let mut barrier = output.recv().await.unwrap();
    let OutboundFrame::Flush(acknowledge) =
        std::mem::replace(&mut barrier.frame, OutboundFrame::Pong(Bytes::new()))
    else {
        panic!("expected flush barrier")
    };
    assert!(!send.is_finished());
    acknowledge.send(Ok(())).unwrap();
    send.await.unwrap().unwrap();
    let flushing_source = source.clone();
    let pending = tokio::spawn(async move { flushing_source.flush().await });
    let barrier = output.recv().await.unwrap();
    assert!(matches!(barrier.frame, OutboundFrame::Flush(_)));
    source.detach();
    assert!(pending.await.unwrap().is_err());
}

#[test]
fn control_validation_preserves_missing_null_and_numeric_semantics() {
    assert!(
        validate_control_message(&json!({"type":"ping","requestId":"p","clientSentAt":1.0}))
            .is_ok()
    );
    assert!(
        validate_control_message(&json!({"type":"ping","requestId":"p","clientSentAt":null}))
            .is_err()
    );
    assert!(validate_control_message(&json!({"type":"session.events.set_subscription.request","requestId":"s","events":[],"notifications":null})).is_err());
    let mut heartbeat = json!({"type":"client_heartbeat","deviceType":"mobile","focusedAgentId":null,"lastActivityAt":"2026-01-01T00:00:00Z","appVisible":false});
    assert!(validate_control_message(&heartbeat).is_ok());
    heartbeat.as_object_mut().unwrap().remove("focusedAgentId");
    assert!(validate_control_message(&heartbeat).is_err());
}

#[test]
fn source_guard_cancels_producers_and_retains_only_ordinary_sessions() {
    for ephemeral in [false, true] {
        let transport = TransportService::new(Weak::new());
        let (source, _) = make_source("physical", true);
        let subscription = make_request(&source, "fetch_agents_request", "watch")
            .begin_subscription("agents", None, None)
            .unwrap();
        let key = serde_json::to_string(&("owner", "client")).unwrap();
        transport.sessions.lock().unwrap().sessions.insert(
            key.clone(),
            Session {
                id: source.session_id().into(),
                permissions: source.source.permissions.clone(),
                sources: [(source.source_id().into(), Arc::downgrade(&source.source))]
                    .into_iter()
                    .collect(),
                retained_until: None,
                ephemeral,
                permission_ceiling: None,
            },
        );
        {
            let _guard = SourceGuard {
                transport: &transport,
                context: &source,
                key: &key,
                completed: false,
            };
        }
        assert!(!source.is_live());
        assert!(subscription.cancellation().is_cancelled());
        let registry = transport.sessions.lock().unwrap();
        if ephemeral {
            assert!(!registry.sessions.contains_key(&key));
        } else {
            let retained = &registry.sessions[&key];
            assert!(retained.sources.is_empty());
            assert!(
                retained
                    .retained_until
                    .unwrap()
                    .duration_since(Instant::now())
                    > Duration::from_secs(89)
            );
        }
    }
}

#[test]
fn bearer_tokens_follow_source_whitespace_and_single_token_rules() {
    assert_eq!(
        extract_http_bearer_token(" \tBearer\t token \r\n"),
        Some("token")
    );
    assert_eq!(
        extract_http_bearer_token("\u{feff}Bearer\u{00a0}token\u{feff}"),
        Some("token")
    );
    for invalid in [
        "Bearer",
        "Bearer ",
        "Bearer token other",
        "bearer token",
        "Basic token",
        "Bearer\u{0085}token",
    ] {
        assert_eq!(extract_http_bearer_token(invalid), None);
    }
}

#[tokio::test]
async fn event_source_selection_requires_demand_authority_and_liveness() {
    let transport = TransportService::new(Weak::new());
    let (source, _) = make_source("catalog", true);
    transport.sessions.lock().unwrap().sessions.insert(
        "owner".into(),
        Session {
            id: source.session_id().into(),
            permissions: source.source.permissions.clone(),
            sources: [(source.source_id().into(), Arc::downgrade(&source.source))]
                .into_iter()
                .collect(),
            retained_until: None,
            ephemeral: false,
            permission_ceiling: None,
        },
    );
    assert!(
        transport
            .event_sources("providers_snapshot_update")
            .is_empty()
    );
    let request = make_request(
        &source,
        "session.events.set_subscription.request",
        "catalog",
    );
    let subscription = request.begin_subscription("events", None, None).unwrap();
    request.register_events(
        &subscription,
        [
            "providers_snapshot_update".to_owned(),
            "status.daemon_config_changed".to_owned(),
        ]
        .into_iter()
        .collect(),
        false,
    );
    subscription.activate().await.unwrap();
    assert_eq!(
        transport.event_sources("providers_snapshot_update").len(),
        1
    );
    assert_eq!(
        transport
            .event_sources("status.daemon_config_changed")
            .len(),
        1
    );
    assert!(transport.event_sources("not_an_event").is_empty());
    source.source.permissions.write().unwrap().clear();
    assert!(
        transport
            .event_sources("providers_snapshot_update")
            .is_empty()
    );
    assert!(
        transport
            .event_sources("status.daemon_config_changed")
            .is_empty()
    );
    source.source.permissions.write().unwrap().extend(
        permissions::OWNER_PERMISSIONS
            .iter()
            .map(|permission| (*permission).to_owned()),
    );
    source.detach();
    assert!(!transport.has_event_demand("providers_snapshot_update"));
}

#[tokio::test]
async fn update_maintenance_reaches_subscribed_ui_but_never_legacy_or_unsubscribed_clients() {
    const OPERATION: &str = "0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11";
    let transport = TransportService::new(Weak::new());
    let (ui, mut ui_output) = make_source("ui", true);
    let (older, mut older_output) = make_source("older-modern", true);
    let (legacy, mut legacy_output) = make_source("legacy", false);
    transport.sessions.lock().unwrap().sessions.insert(
        "owner".into(),
        Session {
            id: ui.session_id().into(),
            permissions: ui.source.permissions.clone(),
            sources: [&ui, &older, &legacy]
                .into_iter()
                .map(|source| (source.source_id().into(), Arc::downgrade(&source.source)))
                .collect(),
            retained_until: None,
            ephemeral: false,
            permission_ceiling: None,
        },
    );
    // The host feed a current Flutter client installs through the real request path.
    let feed = ui.for_request(json!({"type":"session.events.set_subscription.request","requestId":"feed","events":["status.server_info","daemon.update.maintenance"],"notifications":true}));
    assert!(
        transport
            .handle(&feed, feed.request().unwrap())
            .await
            .unwrap()
    );
    assert_eq!(
        json_frame(&ui_output.recv().await.unwrap())["message"]["type"],
        "session.events.set_subscription.response"
    );
    for active in [true, false] {
        transport
            .publish_event(json!({"type":"daemon.update.maintenance","payload":{"operationId":OPERATION,"active":active}}))
            .await
            .unwrap();
        let event = json_frame(&ui_output.recv().await.unwrap());
        assert_eq!(event["message"]["type"], "daemon.update.maintenance");
        assert_eq!(event["message"]["payload"]["operationId"], OPERATION);
        assert_eq!(event["message"]["payload"]["active"], active);
    }
    // Clients that never asked for it (older apps, legacy protocol) are not sent an unknown type.
    assert!(older_output.try_recv().is_err());
    assert!(legacy_output.try_recv().is_err());
    let notice = json!({"type":"daemon.update.maintenance","payload":{}});
    for permission in ["workspace.read", "daemon.read", "hub.execute"] {
        assert!(
            permissions::allows_outbound(&notice, |p| p == permission),
            "{permission}"
        );
    }
    assert!(!permissions::allows_outbound(&notice, |p| p == "tunnel.manage"));
}

#[tokio::test]
async fn subagent_rows_follow_child_membership_but_parent_status_keeps_its_feed() {
    let transport = TransportService::new(Weak::new());
    let make_session = |id: &str| {
        RequestContext::attach(SourceConfig {
            id: id.into(),
            session_id: id.into(),
            client_id: id.into(),
            capabilities: json!({"owned_subscriptions":true,"provider_subagents":true}),
            principal: json!({"id":"owner","kind":"owner"}),
            permissions: Arc::new(RwLock::new(
                permissions::OWNER_PERMISSIONS
                    .iter()
                    .map(|p| (*p).to_owned())
                    .collect(),
            )),
            routed: false,
            app_version: None,
            cancel: CancellationToken::new(),
        })
    };
    let (viewer, mut viewer_output) = make_session("child-viewer");
    let (parent, mut parent_output) = make_session("parent-only");
    let (older, mut older_output) = make_session("older-app");
    for source in [&viewer, &parent, &older] {
        transport.sessions.lock().unwrap().sessions.insert(
            source.session_id().into(),
            Session {
                id: source.session_id().into(),
                permissions: source.source.permissions.clone(),
                sources: [(source.source_id().into(), Arc::downgrade(&source.source))]
                    .into_iter()
                    .collect(),
                retained_until: None,
                ephemeral: false,
                permission_ceiling: None,
            },
        );
        let request = source.for_request(json!({
            "type":"session.events.set_subscription.request","requestId":"feed",
            "events":["agent.provider_subagents.update"]
        }));
        assert!(
            transport
                .handle(&request, request.request().unwrap())
                .await
                .unwrap()
        );
    }
    for output in [&mut viewer_output, &mut parent_output, &mut older_output] {
        assert_eq!(
            json_frame(&output.recv().await.unwrap())["message"]["type"],
            "session.events.set_subscription.response"
        );
    }
    let subscribe = |source: &RequestContext, children: Option<Value>| {
        let mut request = json!({
            "type":"agent.timeline.set_subscription.request","requestId":"timeline",
            "agentIds":["parent"]
        });
        if let Some(children) = children {
            request["subagents"] = children;
        }
        source
            .for_request(request)
            .begin_subscription("timelines", None, None)
            .unwrap()
    };
    let child = subscribe(
        &viewer,
        Some(json!([{"parentAgentId":"parent","subagentId":"child"}])),
    );
    let parent_membership = subscribe(&parent, Some(json!([])));
    let old_membership = subscribe(&older, None);
    child.activate().await.unwrap();
    parent_membership.activate().await.unwrap();
    old_membership.activate().await.unwrap();
    let row = json!({"type":"agent.provider_subagents.update","payload":{
        "kind":"timeline","parentAgentId":"parent","subagentId":"child",
        "provider":"omp","item":{"type":"assistant_message","text":"stored row"},
        "timestamp":"2026-11-18T00:00:00Z","seq":1,"epoch":"child-epoch"
    }});
    transport.publish_event(row.clone()).await.unwrap();
    let delivered = json_frame(&viewer_output.recv().await.unwrap());
    assert_eq!(delivered["message"]["payload"]["kind"], "timeline");
    assert_eq!(
        delivered["message"]["payload"]["subscriptionId"],
        child.response_id()
    );
    assert!(parent_output.try_recv().is_err());
    // An app omitting `subagents` retains its original session-event delivery.
    assert_eq!(
        json_frame(&older_output.recv().await.unwrap())["message"]["payload"]["kind"],
        "timeline"
    );
    let status = json!({"type":"agent.provider_subagents.update","payload":{
        "kind":"upsert","subagent":{"id":"child","parentAgentId":"parent","status":"completed"}
    }});
    transport.publish_event(status).await.unwrap();
    for output in [&mut viewer_output, &mut parent_output, &mut older_output] {
        let delivered = json_frame(&output.recv().await.unwrap());
        assert_eq!(
            delivered["message"]["payload"]["subagent"]["status"],
            "completed"
        );
    }
    // Sibling children and identical child IDs under another parent do not match.
    for (parent_id, child_id) in [("parent", "sibling"), ("other-parent", "child")] {
        let mut other_row = row.clone();
        other_row["payload"]["parentAgentId"] = json!(parent_id);
        other_row["payload"]["subagentId"] = json!(child_id);
        transport.publish_event(other_row).await.unwrap();
        assert!(viewer_output.try_recv().is_err());
        assert!(parent_output.try_recv().is_err());
        older_output.recv().await.unwrap();
    }
    // Closing the final child does not silently restore broadcast delivery.
    child.release().await.unwrap();
    transport.publish_event(row).await.unwrap();
    assert!(viewer_output.try_recv().is_err());
    assert!(parent_output.try_recv().is_err());
    older_output.recv().await.unwrap();
}
