use paseo_protocol::{
    MAX_SAFE_INTEGER, binary_frames::*, helpers::*, providers::*, routing::*, timeline::*, wire::*,
};
use serde_json::{Value, json};

#[test]
fn daemon_release_check_preserves_unknown_latest_and_rejects_path_pins() {
    let request = json!({"type":"daemon.update.check.request","requestId":"check"});
    let response = json!({"type":"daemon.update.check.response","payload":{
        "requestId":"check","serverId":"host","current":{"version":"1.0.0","sourceSha":""},
        "latest":null,"supported":false,"updateAvailable":false,"owner":null,
        "supportError":"Not an owned standalone runtime","checkError":"Release unavailable"
    }});
    for (message, direction) in [
        (request, Direction::Inbound),
        (response, Direction::Outbound),
    ] {
        let parsed = SessionMessage::parse(message.clone(), direction).unwrap();
        let bytes = encode_envelope(&parsed.into_envelope(), direction).unwrap();
        assert!(parse_envelope(&bytes, direction).is_ok());
    }
    let mut submit = json!({"type":"daemon.update.submit.request","requestId":"submit",
        "operationId":"0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11","source":"release",
        "expectedRelease":{"version":"2.0.0","manifestSha512":"A".repeat(88)}});
    assert!(validate_session(&submit, Direction::Inbound).is_ok());
    submit["expectedRelease"]["path"] = json!("/tmp/runtime");
    assert!(validate_session(&submit, Direction::Inbound).is_err());
    submit["expectedRelease"] = json!({"version":"2.0.0","manifestSha512":"short"});
    assert!(validate_session(&submit, Direction::Inbound).is_err());
    submit["expectedRelease"] = Value::Null;
    assert!(validate_session(&submit, Direction::Inbound).is_ok());
}

#[test]
fn daemon_release_check_roundtrips_github_rate_limit() {
    let response = json!({"type":"daemon.update.check.response","payload":{
        "requestId":"check","serverId":"host","targetKind":"daemon",
        "current":{"version":"1.0.0","sourceSha":""},
        "latest":null,"supported":true,"updateAvailable":false,"owner":null,
        "supportError":null,"checkError":"GitHub rate limit",
        "checkErrorCode":"github_rate_limited","retryAt":"2026-11-18T16:37:00Z"
    }});
    let parsed = SessionMessage::parse(response, Direction::Outbound).unwrap();
    let bytes = encode_envelope(&parsed.into_envelope(), Direction::Outbound).unwrap();
    assert!(parse_envelope(&bytes, Direction::Outbound).is_ok());
}

#[test]
fn desktop_app_update_roundtrips_pinned_target_and_authorization_status() {
    let mut submit = json!({"type":"daemon.update.submit.request","requestId":"submit",
        "operationId":"0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11","source":"release",
        "targetKind":"app","forceRunning":false,"idleTimeoutMs":300_000,
        "expectedRelease":{"version":"2.0.0","manifestSha512":"A".repeat(88)}});
    let parsed = SessionMessage::parse(submit.clone(), Direction::Inbound).unwrap();
    let bytes = encode_envelope(&parsed.into_envelope(), Direction::Inbound).unwrap();
    assert!(parse_envelope(&bytes, Direction::Inbound).is_ok());
    submit["expectedRelease"] = Value::Null;
    assert!(validate_session(&submit, Direction::Inbound).is_err());
    submit.as_object_mut().unwrap().remove("expectedRelease");
    assert!(validate_session(&submit, Direction::Inbound).is_err());
    submit["targetKind"] = json!("flutter");
    assert!(validate_session(&submit, Direction::Inbound).is_err());

    for response_type in [
        "daemon.update.submit.response",
        "daemon.update.status.response",
    ] {
        for phase in [
            "admitted",
            "draining",
            "preparation",
            "waitingForParent",
            "waitingForAuthorization",
            "installing",
            "starting",
            "checking",
            "committed",
            "failed",
            "rolledBack",
        ] {
            let terminal = matches!(phase, "committed" | "failed" | "rolledBack");
            let outcome = match phase {
                "committed" => json!("committed"),
                "failed" => json!("failed"),
                "rolledBack" => json!("rolled_back"),
                _ => Value::Null,
            };
            let mut response = json!({"type":response_type,"payload":{"requestId":"status",
                "operation":{"operationId":"op","serverId":"host","targetKind":"app",
                    "phase":phase,"terminal":terminal,"accepted":true,"outcome":outcome,
                    "candidate":{"version":"2.0.0","sourceSha":"abc"},
                    "message":null,"error":null,"recoveryRequired":false,
                    "actionRequired":if phase == "waitingForAuthorization" {json!("osAuthorization")} else {Value::Null}}}});
            let parsed = SessionMessage::parse(response.clone(), Direction::Outbound).unwrap();
            let bytes = encode_envelope(&parsed.into_envelope(), Direction::Outbound).unwrap();
            assert!(
                parse_envelope(&bytes, Direction::Outbound).is_ok(),
                "{phase}"
            );
            response["payload"]["operation"]["currentRoot"] = json!("/private/app");
            assert!(validate_session(&response, Direction::Outbound).is_err());
            response["payload"]["operation"]
                .as_object_mut()
                .unwrap()
                .remove("currentRoot");
            response["payload"]["operation"]["candidate"]["path"] = json!("/private/app");
            assert!(validate_session(&response, Direction::Outbound).is_err());
        }
    }
}

#[test]
fn desktop_app_check_has_bounded_owner_without_runtime_paths() {
    let mut response = json!({"type":"daemon.update.check.response","payload":{
        "requestId":"check","serverId":"host","targetKind":"app",
        "current":{"version":"1.0.0","sourceSha":"abc"},
        "latest":{"version":"2.0.0","manifestSha512":"A".repeat(88)},
        "supported":true,"updateAvailable":true,"owner":{"kind":"desktopApp"},
        "supportError":null,"checkError":null}});
    let parsed = SessionMessage::parse(response.clone(), Direction::Outbound).unwrap();
    let bytes = encode_envelope(&parsed.into_envelope(), Direction::Outbound).unwrap();
    assert!(parse_envelope(&bytes, Direction::Outbound).is_ok());
    response["payload"]["owner"]["path"] = json!("/private/app");
    assert!(validate_session(&response, Direction::Outbound).is_err());
    response["payload"]["owner"] = Value::Null;
    response["payload"]["supported"] = json!(false);
    response["payload"]["latest"] = Value::Null;
    response["payload"]["current"] = Value::Null;
    response["payload"]["supportError"] = json!("Owning app unavailable");
    assert!(validate_session(&response, Direction::Outbound).is_ok());
    let parsed = SessionMessage::parse(response, Direction::Outbound).unwrap();
    let bytes = encode_envelope(&parsed.into_envelope(), Direction::Outbound).unwrap();
    assert!(parse_envelope(&bytes, Direction::Outbound).is_ok());
}

fn component_identity(index: usize) -> Value {
    json!({"version":format!("1.0.{index}"),"sourceSha":format!("{index:040x}"),
        "inputHash":format!("{index:064x}"),"abiVersion":1})
}

fn component_identities() -> Value {
    let mut identities = json!({});
    for (index, id) in ["app", "daemon", "tailnet", "update-helper"]
        .iter()
        .enumerate()
    {
        identities[*id] = component_identity(index);
    }
    identities
}

#[test]
fn desktop_component_submit_roundtrips_exact_selection_and_pinned_collection() {
    let request = json!({"type":"daemon.update.submit.request","requestId":"submit",
        "operationId":"0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11","source":"release",
        "targetKind":"app","selectedComponents":["daemon","update-helper"],
        "forceRunning":false,"idleTimeoutMs":300_000,
        "expectedRelease":{"version":"2.0.0","manifestSha512":"A".repeat(88)}});
    let parsed = SessionMessage::parse(request.clone(), Direction::Inbound).unwrap();
    let bytes = encode_envelope(&parsed.into_envelope(), Direction::Inbound).unwrap();
    assert!(parse_envelope(&bytes, Direction::Inbound).is_ok());
    for selection in [
        json!([]),
        json!(["daemon", "daemon"]),
        json!(["desktop-host"]),
        json!(["app", "daemon", "tailnet", "update-helper", "daemon"]),
        json!(null),
        json!([{"id":"daemon","path":"/private/runtime"}]),
    ] {
        let mut invalid = request.clone();
        invalid["selectedComponents"] = selection;
        assert!(validate_session(&invalid, Direction::Inbound).is_err());
    }
    let mut all_changed = request;
    all_changed
        .as_object_mut()
        .unwrap()
        .remove("selectedComponents");
    assert!(validate_session(&all_changed, Direction::Inbound).is_ok());
}

#[test]
fn desktop_component_check_roundtrips_independent_identities_dependency_and_atomic_plan() {
    let identities = component_identities();
    let response = json!({"type":"daemon.update.check.response","payload":{
        "requestId":"check","serverId":"host","targetKind":"app",
        "current":{"version":"1.0.0","sourceSha":"0".repeat(40)},
        "latest":{"version":"2.0.0","manifestSha512":"A".repeat(88)},
        "supported":true,"updateAvailable":true,"owner":{"kind":"desktopApp"},
        "supportError":null,"checkError":null,"components":identities,
        "componentRequirements":{"app":{"tailnet":1},"daemon":{},"tailnet":{},"update-helper":{}},
        "availableComponents":[{"id":"tailnet","current":identities["tailnet"],
            "target":component_identity(9),"downloadSize":1024,"requires":{},
            "artifactSha256":"a".repeat(64),"activationOnly":false}],
        "restartGroup":"gui","atomicGroups":[["app","tailnet"]] }});
    let parsed = SessionMessage::parse(response.clone(), Direction::Outbound).unwrap();
    let bytes = encode_envelope(&parsed.into_envelope(), Direction::Outbound).unwrap();
    assert!(parse_envelope(&bytes, Direction::Outbound).is_ok());
    for (pointer, value) in [
        ("/payload/components/app/sourceSha", json!("short")),
        ("/payload/components/daemon/inputHash", json!("short")),
        (
            "/payload/availableComponents/0/requires",
            json!({"tailnet":0}),
        ),
        (
            "/payload/availableComponents/0/target/abiVersion",
            json!(-1),
        ),
        ("/payload/restartGroup", json!("installer")),
    ] {
        let mut invalid = response.clone();
        *invalid
            .pointer_mut(pointer)
            .unwrap_or_else(|| panic!("missing {pointer}")) = value;
        assert!(
            validate_session(&invalid, Direction::Outbound).is_err(),
            "{pointer}"
        );
    }
    for parent in [
        "/payload",
        "/payload/current",
        "/payload/components/app",
        "/payload/availableComponents/0",
        "/payload/availableComponents/0/target",
    ] {
        let mut invalid = response.clone();
        invalid.pointer_mut(parent).unwrap()["path"] = json!("/private/runtime");
        assert!(
            validate_session(&invalid, Direction::Outbound).is_err(),
            "{parent}"
        );
    }
    let mut no_change = response;
    no_change["payload"]["updateAvailable"] = json!(false);
    no_change["payload"]["availableComponents"] = json!([]);
    no_change["payload"]["restartGroup"] = json!("none");
    assert!(validate_session(&no_change, Direction::Outbound).is_ok());
}

#[test]
fn desktop_component_operation_roundtrips_restart_groups_and_observed_selection_without_paths() {
    for (selected, restart) in [
        (vec!["update-helper"], "none"),
        (vec!["daemon"], "daemon"),
        (vec!["app"], "gui"),
        (vec!["tailnet"], "gui"),
        (vec![], "none"),
    ] {
        for kind in [
            "daemon.update.submit.response",
            "daemon.update.status.response",
        ] {
            let response = json!({"type":kind,"payload":{"requestId":"status",
                "operation":{"operationId":"op","serverId":"host","targetKind":"app",
                    "phase":"committed","terminal":true,"accepted":true,"outcome":"committed",
                    "candidate":{"version":"2.0.0","sourceSha":"a".repeat(40)},
                    "expectedRelease":{"version":"2.0.0","manifestSha512":"A".repeat(88)},
                    "selectedComponents":selected,"targetComponents":component_identities(),
                    "observedComponents":component_identities(),"restartGroup":restart,
                    "message":null,"error":null,"recoveryRequired":false,"actionRequired":null}}});
            let parsed = SessionMessage::parse(response.clone(), Direction::Outbound).unwrap();
            let bytes = encode_envelope(&parsed.into_envelope(), Direction::Outbound).unwrap();
            assert!(
                parse_envelope(&bytes, Direction::Outbound).is_ok(),
                "{restart}"
            );
            for parent in [
                "/payload/operation",
                "/payload/operation/targetComponents/app",
                "/payload/operation/observedComponents/daemon",
                "/payload/operation/expectedRelease",
            ] {
                let mut invalid = response.clone();
                invalid.pointer_mut(parent).unwrap()["ownerToken"] = json!("a".repeat(64));
                assert!(
                    validate_session(&invalid, Direction::Outbound).is_err(),
                    "{parent}"
                );
            }
            let mut waiting = response;
            waiting["payload"]["operation"]["terminal"] = json!(false);
            waiting["payload"]["operation"]["phase"] = json!("waitingForAuthorization");
            waiting["payload"]["operation"]["outcome"] = Value::Null;
            waiting["payload"]["operation"]["observedComponents"] = Value::Null;
            waiting["payload"]["operation"]["actionRequired"] = json!("osAuthorization");
            assert!(validate_session(&waiting, Direction::Outbound).is_ok());
            waiting["payload"]["operation"]["selectedComponents"] = Value::Null;
            let parsed = SessionMessage::parse(waiting, Direction::Outbound).unwrap();
            let bytes = encode_envelope(&parsed.into_envelope(), Direction::Outbound).unwrap();
            assert!(parse_envelope(&bytes, Direction::Outbound).is_ok());
        }
    }
}

#[test]
fn terminal_wire_bytes_and_borrowed_demultiplexing() {
    let bytes = encode_terminal_frame(TerminalOpcode::Input, 255, b"\x1b[A");
    assert_eq!(bytes, [2, 255, 27, 91, 65]);
    let BinaryFrame::Terminal(frame) = decode_binary_frame(&bytes).unwrap() else {
        panic!("terminal frame")
    };
    assert_eq!(frame.payload.as_ptr(), bytes[2..].as_ptr());
    assert_eq!(frame.slot, 255);
    for bytes in [&[][..], &[1], &[0, 0], &[6, 0]] {
        assert!(decode_binary_frame(bytes).is_err());
    }
    let resize = TerminalResize {
        rows: 24,
        cols: 80,
        intent: Some(ResizeIntent::Claim),
    };
    assert_eq!(
        decode_terminal_resize(&encode_terminal_resize(&resize).unwrap()).unwrap(),
        resize
    );
    for body in [
        br#"{"rows":0,"cols":80}"#.as_slice(),
        br#"{"rows":1.5,"cols":80}"#.as_slice(),
        br#"{"rows":24,"cols":80,"intent":"force"}"#.as_slice(),
    ] {
        assert!(decode_terminal_resize(body).is_err());
    }
}

#[test]
fn file_frames_use_utf8_byte_lengths_and_network_order_metadata() {
    let chunk = FileTransferFrame::Chunk {
        request_id: "é",
        payload: b"\x00\xff",
    };
    let bytes = encode_file_transfer_frame(&chunk).unwrap();
    assert_eq!(bytes, [0x11, 2, 0xc3, 0xa9, 0, 255]);
    assert_eq!(decode_file_transfer_frame(&bytes).unwrap(), chunk);
    let metadata = FileBeginMetadata {
        mime: "text/plain".into(),
        size: 2,
        encoding: FileEncoding::Utf8,
        modified_at: "2026-01-01T00:00:00Z".into(),
        revision: Some("r1".into()),
        file_name: Some("é.txt".into()),
    };
    let begin = FileTransferFrame::Begin {
        request_id: "r",
        metadata,
    };
    let bytes = encode_file_transfer_frame(&begin).unwrap();
    assert_eq!(
        usize::from(u16::from_be_bytes([bytes[3], bytes[4]])),
        bytes.len() - 5
    );
    assert_eq!(decode_file_transfer_frame(&bytes).unwrap(), begin);
    for end in 0..bytes.len() {
        assert!(
            decode_file_transfer_frame(&bytes[..end]).is_err(),
            "prefix {end}"
        );
    }
    let mut extra = bytes;
    extra.push(0);
    assert!(decode_file_transfer_frame(&extra).is_err());
    for malformed in [
        &[0x11, 0][..],
        &[0x11, 2, b'a'],
        &[0x11, 1, 255],
        &[0x12, 1, b'a', 0],
    ] {
        assert!(decode_file_transfer_frame(malformed).is_err());
    }
    assert!(
        encode_file_transfer_frame(&FileTransferFrame::End {
            request_id: &"x".repeat(256)
        })
        .is_err()
    );
}

#[test]
fn routed_frames_have_exact_lengths_and_preserve_nested_payloads() {
    let nested = encode_terminal_frame(TerminalOpcode::Output, 7, b"hello");
    let bytes = encode_host_route_frame("é", &nested).unwrap();
    assert_eq!(&bytes[..5], &[0x30, 0, 2, 0xc3, 0xa9]);
    let frame = decode_host_route_frame(&bytes).unwrap();
    assert_eq!(frame.route_id, "é");
    assert_eq!(frame.payload, nested);
    assert!(matches!(
        decode_binary_frame(frame.payload).unwrap(),
        BinaryFrame::Terminal(_)
    ));
    assert!(encode_host_route_frame(&"é".repeat(128), &[]).is_ok());
    assert!(encode_host_route_frame(&"é".repeat(129), &[]).is_err());
    for malformed in [
        &[0x30, 0][..],
        &[0x30, 0, 0],
        &[0x30, 0, 2, b'a'],
        &[0x30, 0, 1, 255],
        &[0x30, 1, 1],
    ] {
        assert!(decode_host_route_frame(malformed).is_err());
    }
}

#[test]
fn full_schema_catalog_retains_extensions_and_rejects_nested_errors() {
    assert!(message_types(Direction::Inbound).count() > 200);
    assert!(message_types(Direction::Outbound).count() > 200);
    let raw = json!({"type":"fetch_agent_timeline_request","agentId":"a","requestId":"r","cursor":{"epoch":"e","seq":3},"projection":"canonical","limit":0,"observationOnly":true,"futureField":{"nested":true}});
    let message = SessionMessage::parse(raw.clone(), Direction::Inbound).unwrap();
    assert_eq!(message.request_id(), Some("r"));
    assert_eq!(message.clone().into_value(), raw);
    let envelope = message.into_envelope();
    let bytes = encode_envelope(&envelope, Direction::Inbound).unwrap();
    assert_eq!(
        parse_envelope(&bytes, Direction::Inbound).unwrap(),
        envelope
    );
    let mut invalid_observation = raw.clone();
    invalid_observation["observationOnly"] = json!("true");
    assert!(validate_session(&invalid_observation, Direction::Inbound).is_err());
    let mut invalid = raw;
    invalid["cursor"]["seq"] = json!(-1);
    assert!(validate_session(&invalid, Direction::Inbound).is_err());
    invalid["cursor"]["seq"] = json!(MAX_SAFE_INTEGER + 1);
    assert!(validate_session(&invalid, Direction::Inbound).is_err());
    assert!(validate_session(&json!({"type":"plugin.list.request"}), Direction::Inbound).is_err());
    assert!(
        parse_envelope(
            br#"{"type":"session","message":{"type":"unknown"}}"#,
            Direction::Inbound
        )
        .is_err()
    );
    assert!(parse_envelope(br#"{"type":"pong"}"#, Direction::Inbound).is_err());
    assert!(parse_envelope(br#"{"type":"pong"}"#, Direction::Outbound).is_ok());
}

#[test]
fn terminal_and_listing_metadata_remain_optional_and_lossless() {
    let old_exit = json!({"type":"terminal_stream_exit","payload":{"terminalId":"t"}});
    let mut new_exit = old_exit.clone();
    new_exit["payload"]["exitCode"] = json!(7);
    let old_listing = json!({"type":"file_explorer_response","payload":{
        "cwd":"/work","path":".","mode":"list","directory":{"path":".","entries":[]},
        "file":null,"error":null,"requestId":"r"
    }});
    let mut new_listing = old_listing.clone();
    new_listing["payload"]["directory"]["absolutePath"] = json!("/work");
    for value in [old_exit, new_exit.clone(), old_listing, new_listing.clone()] {
        let message = SessionMessage::parse(value.clone(), Direction::Outbound).unwrap();
        assert_eq!(message.clone().into_value(), value);
        let envelope = message.into_envelope();
        let bytes = encode_envelope(&envelope, Direction::Outbound).unwrap();
        assert_eq!(
            parse_envelope(&bytes, Direction::Outbound).unwrap(),
            envelope
        );
    }
    new_exit["payload"]["exitCode"] = json!("7");
    assert!(validate_session(&new_exit, Direction::Outbound).is_err());
    new_listing["payload"]["directory"]["absolutePath"] = json!(false);
    assert!(validate_session(&new_listing, Direction::Outbound).is_err());
    let mut snapshot =
        json!({"rows":1,"cols":1,"grid":[[]],"scrollback":[],"cursor":{"row":0,"col":0}});
    assert!(validate_terminal_snapshot(&snapshot).is_ok());
    snapshot["alternateScreen"] = json!(true);
    assert!(validate_terminal_snapshot(&snapshot).is_ok());
    snapshot["alternateScreen"] = json!("true");
    assert!(validate_terminal_snapshot(&snapshot).is_err());
}

#[test]
fn native_plugin_review_targets_are_complete_and_npm_writes_are_rejected() {
    let identity = json!({"kind":"cargo","crateName":"paseo-demo","pluginPath":""});
    let target = json!({"kind":"cargo","version":"1.2.3","checksum":"a".repeat(64),"resolved":"https://static.crates.io/crates/paseo-demo/paseo-demo-1.2.3.crate"});
    let mut message = json!({"type":"plugin.source.update.apply.request","requestId":"r","proposals":[{"id":"demo","expected":{"identity":identity,"installationRoot":"/plugins/demo","revision":"1.2.2","artifact":target.clone()},"target":target}]});
    validate_session(&message, Direction::Inbound).unwrap();
    message["proposals"][0]["target"]["checksum"] = json!("bad");
    assert!(validate_session(&message, Direction::Inbound).is_err());
    message["proposals"][0]["target"] = json!({"kind":"release","version":"1.2.3","url":"https://example.test/plugin.tar.gz","sha256":"b".repeat(64),"format":"tar.gz"});
    validate_session(&message, Direction::Inbound).unwrap();
    message["proposals"][0]["target"]["url"] = json!("http://example.test/insecure");
    assert!(validate_session(&message, Direction::Inbound).is_err());
    let npm = json!({"type":"plugin.source.update.preview.request","requestId":"r","target":{"kind":"npm","version":"1.0"}});
    assert!(validate_session(&npm, Direction::Inbound).is_err());
}

#[test]
fn snapshot_metadata_is_deduplicated_and_plugins_remain_open() {
    let entries = vec![
        json!({"provider":"custom-plugin","extension":true,"models":[{"id":"a","provider":"custom-plugin","thinkingOptions":[{"id":"deep","label":"Deep"}],"defaultThinkingOptionId":"deep"},{"id":"b","provider":"custom-plugin","thinkingOptions":[{"id":"deep","label":"Deep"}],"defaultThinkingOptionId":"deep"}]}),
    ];
    let compact = compact_provider_snapshot(entries.clone()).unwrap();
    assert_eq!(compact.thinking_sets.len(), 1);
    assert_eq!(compact.entries[0]["models"][1]["thinkingSet"], 0);
    assert_eq!(expand_provider_snapshot(compact).unwrap(), entries);
    assert!(
        expand_provider_snapshot(CompactProviderSnapshot {
            entries: vec![json!({"provider":"p","models":[{"thinkingSet":1}]})],
            thinking_sets: vec![]
        })
        .is_err()
    );
    assert!(is_builtin_provider("claude"));
    assert!(!is_builtin_provider("arbitrary-plugin"));
    assert!(!definitions().builtins.is_empty());
}

#[test]
fn cursor_epochs_and_subscription_keys_never_alias() {
    let a = TimelineCursor::new("one", 2).unwrap();
    let b = TimelineCursor::new("two", 2).unwrap();
    assert_eq!(a.compare(&b), None);
    assert_eq!(
        a.compare(&TimelineCursor::new("one", 3).unwrap()),
        Some(std::cmp::Ordering::Less)
    );
    assert!(TimelineCursor::parse(json!({"epoch":"e","seq":-1})).is_err());
    assert!(
        serde_json::from_value::<TimelineCursor>(json!({"epoch":"e","seq":9007199254740992_u64}))
            .is_err()
    );
    assert_eq!(
        timeline_item_identity(&json!({"type":"plugin","pluginId":"demo","id":"x"})),
        Some("demo/x".into())
    );
    assert_eq!(terminal_subscription_key("/repo", Some("w")), "w::/repo");
    assert_eq!(terminal_subscription_key("/repo", None), "/repo");
}

#[test]
fn ipv6_and_relay_urls_preserve_security_and_routing_identity() {
    assert_eq!(normalize_host_port(" [::1]:06767 ").unwrap(), "[::1]:6767");
    assert_eq!(
        normalize_loopback_to_localhost("[::1]:6767").unwrap(),
        "localhost:6767"
    );
    let url = build_relay_websocket_url(
        "relay.example:443",
        true,
        "server +/é",
        RelayRole::Client,
        None,
        None,
    )
    .unwrap();
    assert_eq!(url.path(), "/ws");
    assert_eq!(
        url.query_pairs().find(|(k, _)| k == "serverId").unwrap().1,
        "server +/é"
    );
    assert_eq!(
        extract_host_port_from_websocket_url("wss://[::1]/ws/").unwrap(),
        "[::1]:443"
    );
    let relay=parse_relay_connection_uri("relay://[::1]:443/server%20%C3%A9?key=a%2Bb%3D&ssl=true&security=protected&password=private").unwrap();
    assert_eq!(relay.offer.server_id, "server é");
    assert_eq!(
        relay.offer.minimum_security_mode,
        Some(MinimumSecurityMode::Protected)
    );
    assert_eq!(
        parse_relay_connection_uri(&serialize_relay_connection_uri(&relay).unwrap()).unwrap(),
        relay
    );
    assert!(!format!("{relay:?}").contains("private"));
    for bad in [
        "relay://host:443/a?key=x&security=legacy",
        "relay://u:p@host:443/a?key=x",
        "relay://host:443/a%2Fb?key=x",
        "relay://host/a?key=x",
    ] {
        assert!(parse_relay_connection_uri(bad).is_err());
    }
}

#[test]
fn shared_offers_and_connection_storage_do_not_leak_passwords() {
    let parts = parse_connection_uri("tcp://localhost:6767?ssl=true&password=private").unwrap();
    assert!(
        !serialize_connection_uri(&parts, false)
            .unwrap()
            .contains("private")
    );
    assert_eq!(
        parse_connection_uri(&serialize_connection_uri(&parts, true).unwrap()).unwrap(),
        parts
    );
    let offer = ConnectionOffer {
        v: 2,
        server_id: "server".into(),
        daemon_public_key_b64: "key".into(),
        relay: RelayEndpoint {
            endpoint: "relay.paseo.sh:443".into(),
            use_tls: Some(true),
        },
        minimum_security_mode: Some(MinimumSecurityMode::Protected),
    };
    let link = format!("https://paseo.sh/#offer={}", offer.encode().unwrap());
    assert_eq!(parse_connection_offer_from_url(&link).unwrap(), Some(offer));
    assert_eq!(parse_connection_offer_from_url("no-offer").unwrap(), None);
    assert!(parse_connection_offer_from_url("#offer=%%%%").is_err());
    assert!(
        ConnectionOffer::parse(
            br#"{"v":1,"serverId":"s","daemonPublicKeyB64":"k","relay":{"endpoint":"r"}}"#
        )
        .is_err()
    );
}

#[test]
fn errors_remain_redacted_and_helpers_follow_wire_conventions() {
    let error = validate_session(
        &json!({"type":"plugin.list.request","requestId":{"secret":"private"}}),
        Direction::Inbound,
    )
    .unwrap_err();
    assert!(!error.to_string().contains("private"));
    assert_eq!(error_message(&Value::Null), "null");
    assert_eq!(
        error_message(&json!({"message":"failure"})),
        "[object Object]"
    );
    assert!(validate_branch_slug("feature/add-api").is_ok());
    assert!(validate_branch_slug("Bad Branch").is_err());
}

#[test]
fn browser_cross_field_refinements_survive_schema_migration() {
    let mut message = json!({"type":"browser.automation.execute.request","requestId":"r","command":{"command":"wait","args":{"browserId":"00000000-0000-4000-8000-000000000001","text":"ready"}}});
    assert!(validate_session(&message, Direction::Outbound).is_ok());
    message["command"]["args"]["url"] = json!("https://example.test");
    assert!(validate_session(&message, Direction::Outbound).is_err());
    message["command"] = json!({"command":"navigate","args":{"browserId":"00000000-0000-4000-8000-000000000001","url":"https://example.test"}});
    assert!(validate_session(&message, Direction::Outbound).is_ok());
    message["command"]["args"]["url"] = json!("file:///private");
    assert!(validate_session(&message, Direction::Outbound).is_err());
}

#[test]
fn selected_commit_rpc_validates_session_and_websocket_boundaries() {
    let request = json!({
        "type":"checkout.commit_selected.request", "cwd":"/repo",
        "paths":["name with spaces.txt", ":(glob)*"], "message":"Summary\n\nBody",
        "requestId":"selected-1",
    });
    let assert_valid = |message: Value, direction| {
        validate_session(&message, direction).unwrap();
        let envelope = json!({"type":"session","message":message});
        parse_envelope(&serde_json::to_vec(&envelope).unwrap(), direction).unwrap();
    };
    assert_valid(request.clone(), Direction::Inbound);
    for (field, value) in [
        ("paths", json!([])),
        ("paths", json!([""])),
        ("paths", json!([4])),
        ("message", json!("")),
        ("message", json!(" \n\t")),
        ("message", Value::Null),
    ] {
        let mut invalid = request.clone();
        invalid[field] = value;
        assert!(validate_session(&invalid, Direction::Inbound).is_err());
        assert!(
            parse_envelope(
                &serde_json::to_vec(&json!({"type":"session","message":invalid})).unwrap(),
                Direction::Inbound,
            )
            .is_err()
        );
    }
    let success = json!({
        "type":"checkout.commit_selected.response",
        "payload":{"cwd":"/repo","success":true,"error":null,"commitSha":"abc123","requestId":"selected-1"},
    });
    assert_valid(success.clone(), Direction::Outbound);
    let mut missing_sha = success;
    missing_sha["payload"]
        .as_object_mut()
        .unwrap()
        .remove("commitSha");
    assert!(validate_session(&missing_sha, Direction::Outbound).is_err());
    assert_valid(
        json!({
            "type":"checkout.commit_selected.response",
            "payload":{"cwd":"/repo","success":false,"error":{"code":"UNKNOWN","message":"Select at least one changed file"},"requestId":"selected-1"},
        }),
        Direction::Outbound,
    );
    // The new operation does not narrow the legacy message/generation contract.
    assert_valid(
        json!({
            "type":"checkout_commit_request","cwd":"/repo","requestId":"legacy",
        }),
        Direction::Inbound,
    );
}

#[test]
fn retired_worktree_requests_and_refusals_remain_wire_compatible() {
    let request = json!({
        "type":"create_paseo_worktree_request","requestId":"old-client",
        "cwd":"/existing/repo","action":"branch-off","worktreeSlug":"feature","refName":"main"
    });
    assert_eq!(
        SessionMessage::parse(request.clone(), Direction::Inbound)
            .unwrap()
            .into_value(),
        request
    );
    for response in [
        json!({"type":"create_paseo_worktree_response","payload":{
            "requestId":"old-client","workspace":null,"setupTerminalId":null,
            "errorCode":"unsupported","error":"Managed worktrees are no longer supported"
        }}),
        json!({"type":"paseo_worktree_archive_response","payload":{
            "requestId":"old-client","success":false,"removedAgents":[],
            "error":{"code":"UNKNOWN","message":"Managed worktrees are no longer supported"}
        }}),
        json!({"type":"paseo_worktree_list_response","payload":{
            "requestId":"old-client","worktrees":[],
            "error":{"code":"UNKNOWN","message":"Managed worktrees are no longer supported"}
        }}),
    ] {
        assert_eq!(
            SessionMessage::parse(response.clone(), Direction::Outbound)
                .unwrap()
                .into_value(),
            response
        );
    }
}
