use paseo_protocol::{
    MAX_SAFE_INTEGER, binary_frames::*, helpers::*, providers::*, routing::*, timeline::*, wire::*,
};
use serde_json::{Value, json};

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
