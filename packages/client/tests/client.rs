use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use paseo_client::{
    ClientError, ConnectOptions, CursorPages, DaemonClient, DaemonTarget, ReconnectingClient,
    RequestOptions, SubscriptionEvent,
};
use paseo_protocol::binary_frames::{
    FileBeginMetadata, FileEncoding, FileTransferFrame, decode_file_transfer_frame,
    encode_file_transfer_frame,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::oneshot,
};
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

type Socket = WebSocketStream<TcpStream>;
const DEADLINE: Duration = Duration::from_secs(5);

async fn fixture() -> Result<(TcpListener, ConnectOptions, tempfile::TempDir)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let home = tempfile::tempdir()?;
    let mut options = ConnectOptions::new(DaemonTarget::Endpoint {
        host: format!("ws://{}", listener.local_addr()?),
    });
    options.home = Some(home.path().into());
    options.client_id = Some("cid_sdk_test".into());
    options.local_credential = Some("isolated-test-credential".into());
    options.timeout = DEADLINE;
    Ok((listener, options, home))
}

async fn receive(socket: &mut Socket) -> Result<Value> {
    loop {
        match tokio::time::timeout(DEADLINE, socket.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("socket closed"))??
        {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Ping(data) => socket.send(Message::Pong(data)).await?,
            other => anyhow::bail!("unexpected frame: {other:?}"),
        }
    }
}

async fn reply(socket: &mut Socket, kind: &str, payload: Value) -> Result<()> {
    socket
        .send(Message::Text(
            json!({"type":"session","message":{"type":kind,"payload":payload}})
                .to_string()
                .into(),
        ))
        .await?;
    Ok(())
}

async fn accept(listener: &TcpListener) -> Result<Socket> {
    let (stream, _) = tokio::time::timeout(DEADLINE, listener.accept()).await??;
    let mut socket = accept_async(stream).await?;
    let hello = receive(&mut socket).await?;
    assert_eq!(hello["type"], "hello");
    assert_eq!(
        hello["auth"],
        json!({"kind":"localCredential","token":"isolated-test-credential"})
    );
    reply(&mut socket, "status", json!({"status":"server_info","protocolVersion":1,"features":{"ownedSubscriptions":true,"daemonStatusRpc":true,"creationLifecycle":true}})).await?;
    Ok(socket)
}

fn code(error: &anyhow::Error) -> &str {
    &error
        .downcast_ref::<ClientError>()
        .expect("structured client error")
        .code
}

#[tokio::test]
async fn correlates_out_of_order_and_preserves_rpc_errors() -> Result<()> {
    let (listener, options, _home) = fixture().await?;
    let (release, held) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = accept(&listener).await?;
        let first = receive(&mut socket).await?["message"].clone();
        let second = receive(&mut socket).await?["message"].clone();
        reply(
            &mut socket,
            "test.response",
            json!({"requestId":second["requestId"],"value":second["value"]}),
        )
        .await?;
        reply(
            &mut socket,
            "test.response",
            json!({"requestId":first["requestId"],"value":first["value"]}),
        )
        .await?;
        let failed = receive(&mut socket).await?["message"].clone();
        reply(&mut socket, "rpc_error", json!({"requestId":failed["requestId"],"code":"DENIED","message":"not permitted","detail":17})).await?;
        let _ = held.await;
        Ok::<_, anyhow::Error>(())
    });
    let client = DaemonClient::connect(options, CancellationToken::new()).await?;
    let (first, second) = tokio::join!(
        client.request(
            json!({"type":"test.request","value":1}),
            "test.response",
            DEADLINE
        ),
        client.request(
            json!({"type":"test.request","value":2}),
            "test.response",
            DEADLINE
        ),
    );
    assert_eq!(first?["value"], 1);
    assert_eq!(second?["value"], 2);
    let error = client
        .request(json!({"type":"test.request"}), "test.response", DEADLINE)
        .await
        .unwrap_err();
    assert_eq!(code(&error), "DENIED");
    assert_eq!(
        error
            .downcast_ref::<ClientError>()
            .unwrap()
            .details
            .as_ref()
            .unwrap()["detail"],
        17
    );
    client.close().await;
    let _ = release.send(());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn cancelled_and_expired_requests_release_correlation_without_closing_connection()
-> Result<()> {
    let (listener, options, _home) = fixture().await?;
    let cancellation = CancellationToken::new();
    let server_cancel = cancellation.clone();
    let (release, held) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = accept(&listener).await?;
        let cancelled = receive(&mut socket).await?["message"].clone();
        assert_eq!(cancelled["requestId"], "reuse-after-cancel");
        server_cancel.cancel();
        let expired = receive(&mut socket).await?["message"].clone();
        assert_eq!(expired["requestId"], "reuse-after-timeout");
        for _ in 0..2 {
            let request = receive(&mut socket).await?["message"].clone();
            reply(
                &mut socket,
                "test.response",
                json!({"requestId":request["requestId"],"ok":true}),
            )
            .await?;
        }
        let _ = held.await;
        Ok::<_, anyhow::Error>(())
    });
    let client = DaemonClient::connect(options, CancellationToken::new()).await?;
    let error = client
        .request_cancellable(
            json!({"type":"test.request","requestId":"reuse-after-cancel"}),
            "test.response",
            DEADLINE,
            cancellation,
        )
        .await
        .unwrap_err();
    assert_eq!(code(&error), "DAEMON_REQUEST_CANCELLED");
    let error = client
        .request(
            json!({"type":"test.request","requestId":"reuse-after-timeout"}),
            "test.response",
            Duration::from_millis(30),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&error), "DAEMON_REQUEST_TIMEOUT");
    for id in ["reuse-after-cancel", "reuse-after-timeout"] {
        assert_eq!(
            client
                .request(
                    json!({"type":"test.request","requestId":id}),
                    "test.response",
                    DEADLINE
                )
                .await?["ok"],
            true
        );
    }
    client.close().await;
    let _ = release.send(());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn binary_download_and_upload_preserve_bytes() -> Result<()> {
    let (listener, options, _home) = fixture().await?;
    let (release, held) = oneshot::channel();
    let bytes = vec![0, 255, 13, 10, 128, 42];
    let expected = bytes.clone();
    let server = tokio::spawn(async move {
        let mut socket = accept(&listener).await?;
        let read = receive(&mut socket).await?["message"].clone();
        assert_eq!(read["acceptBinary"], true);
        let id = read["requestId"].as_str().unwrap();
        for frame in [
            FileTransferFrame::Begin {
                request_id: id,
                metadata: FileBeginMetadata {
                    mime: "application/octet-stream".into(),
                    size: expected.len() as u64,
                    encoding: FileEncoding::Binary,
                    modified_at: "2026-01-01T00:00:00Z".into(),
                    revision: Some("revision-a".into()),
                    file_name: None,
                },
            },
            FileTransferFrame::Chunk {
                request_id: id,
                payload: &expected,
            },
            FileTransferFrame::End { request_id: id },
        ] {
            socket
                .send(Message::Binary(encode_file_transfer_frame(&frame)?.into()))
                .await?;
        }
        let upload = receive(&mut socket).await?["message"].clone();
        assert_eq!(upload["type"], "file.upload.request");
        let id = upload["requestId"].as_str().unwrap();
        let mut received = Vec::new();
        loop {
            let frame = tokio::time::timeout(DEADLINE, socket.next())
                .await?
                .unwrap()?;
            let Message::Binary(frame) = frame else {
                anyhow::bail!("expected binary upload")
            };
            match decode_file_transfer_frame(&frame)? {
                FileTransferFrame::Begin {
                    request_id,
                    metadata,
                } => {
                    assert_eq!(request_id, id);
                    assert_eq!(metadata.size, expected.len() as u64);
                }
                FileTransferFrame::Chunk {
                    request_id,
                    payload,
                } => {
                    assert_eq!(request_id, id);
                    received.extend_from_slice(payload);
                }
                FileTransferFrame::End { request_id } => {
                    assert_eq!(request_id, id);
                    break;
                }
            }
        }
        assert_eq!(received, expected);
        reply(
            &mut socket,
            "file.upload.response",
            json!({"requestId":id,"path":"/uploads/blob.bin","error":null}),
        )
        .await?;
        let _ = held.await;
        Ok::<_, anyhow::Error>(())
    });
    let client = DaemonClient::connect(options, CancellationToken::new()).await?;
    let file = client
        .read_file("/test", "blob.bin", 1024, RequestOptions::default())
        .await?;
    assert_eq!(&file.bytes[..], bytes.as_slice());
    assert_eq!(file.metadata.revision.as_deref(), Some("revision-a"));
    assert_eq!(
        client
            .upload_file(
                "blob.bin",
                "application/octet-stream",
                "2026-01-01T00:00:00Z",
                &bytes,
                RequestOptions::default()
            )
            .await?["path"],
        "/uploads/blob.bin"
    );
    client.close().await;
    let _ = release.send(());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn pagination_and_owned_updates_retain_bootstrap_races() -> Result<()> {
    let (listener, options, _home) = fixture().await?;
    let (release, held) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = accept(&listener).await?;
        for index in 0..2 {
            let request = receive(&mut socket).await?["message"].clone();
            assert_eq!(request["page"]["limit"], 100);
            assert_eq!(
                request["page"]["cursor"],
                if index == 0 {
                    Value::Null
                } else {
                    json!("opaque-next")
                }
            );
            reply(&mut socket, "fetch_agents_response", json!({"requestId":request["requestId"],"entries":[{"agent":{"id":index}}],"pageInfo":{"nextCursor":if index==0 {json!("opaque-next")}else{Value::Null},"hasMore":index==0}})).await?;
        }
        let request = receive(&mut socket).await?["message"].clone();
        reply(
            &mut socket,
            "agent_update",
            json!({"subscriptionId":"sub-test","agent":{"id":"changed"}}),
        )
        .await?;
        reply(&mut socket, "fetch_agents_response", json!({"requestId":request["requestId"],"subscriptionId":"sub-test","entries":[],"pageInfo":{"hasMore":false}})).await?;
        let release = receive(&mut socket).await?["message"].clone();
        assert_eq!(release["type"], "subscription.release.request");
        assert_eq!(release["subscriptionId"], "sub-test");
        reply(
            &mut socket,
            "subscription.release.response",
            json!({"requestId":release["requestId"],"released":true}),
        )
        .await?;
        let _ = held.await;
        Ok::<_, anyhow::Error>(())
    });
    let client = Arc::new(DaemonClient::connect(options, CancellationToken::new()).await?);
    let mut pages = CursorPages::agents(json!({}));
    assert_eq!(
        pages
            .next(&client, RequestOptions::default())
            .await?
            .unwrap()["entries"][0]["agent"]["id"],
        0
    );
    assert_eq!(
        pages
            .next(&client, RequestOptions::default())
            .await?
            .unwrap()["entries"][0]["agent"]["id"],
        1
    );
    assert!(
        pages
            .next(&client, RequestOptions::default())
            .await?
            .is_none()
    );
    let parent_cancel = CancellationToken::new();
    let mut subscription = client
        .observe_agents(
            json!({}),
            RequestOptions {
                timeout: DEADLINE,
                cancel: parent_cancel.clone(),
            },
        )
        .await?;
    assert_eq!(
        subscription.recv().await?["payload"]["agent"]["id"],
        "changed"
    );
    subscription.release().await?;
    subscription.release().await?;
    assert!(!parent_cancel.is_cancelled());
    client.close().await;
    let _ = release.send(());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn reconnect_restores_snapshot_without_replaying_mutations() -> Result<()> {
    let (listener, options, _home) = fixture().await?;
    let (disconnect, disconnected) = oneshot::channel();
    let (release, held) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut first = accept(&listener).await?;
        let request = receive(&mut first).await?["message"].clone();
        reply(
            &mut first,
            "fetch_agents_response",
            json!({"requestId":request["requestId"],"subscriptionId":"first","entries":[]}),
        )
        .await?;
        let _ = disconnected.await;
        first.close(None).await?;
        drop(first);
        let mut second = accept(&listener).await?;
        let request = receive(&mut second).await?["message"].clone();
        assert_eq!(request["type"], "fetch_agents_request");
        reply(
            &mut second,
            "fetch_agents_response",
            json!({"requestId":request["requestId"],"subscriptionId":"second","entries":[]}),
        )
        .await?;
        let request = receive(&mut second).await?["message"].clone();
        assert_eq!(request["type"], "subscription.release.request");
        reply(
            &mut second,
            "subscription.release.response",
            json!({"requestId":request["requestId"],"released":true}),
        )
        .await?;
        let _ = held.await;
        Ok::<_, anyhow::Error>(())
    });
    let mut client = ReconnectingClient::connect(options, CancellationToken::new()).await?;
    let mut subscription = client.observe(
        "fetch_agents_request",
        "fetch_agents_response",
        json!({"subscribe":{}}),
        RequestOptions::default(),
    );
    assert!(matches!(
        tokio::time::timeout(DEADLINE, subscription.recv())
            .await?
            .unwrap()?,
        SubscriptionEvent::Snapshot {
            restored: false,
            ..
        }
    ));
    let _ = disconnect.send(());
    assert!(matches!(
        tokio::time::timeout(DEADLINE, subscription.recv())
            .await?
            .unwrap()?,
        SubscriptionEvent::Snapshot { restored: true, .. }
    ));
    subscription.release().await;
    client.close().await;
    let _ = release.send(());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn dropping_request_future_releases_its_identifier() -> Result<()> {
    let (listener, options, _home) = fixture().await?;
    let (submitted, observed) = oneshot::channel();
    let (release, held) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = accept(&listener).await?;
        let first = receive(&mut socket).await?["message"].clone();
        assert_eq!(first["requestId"], "dropped-request");
        let _ = submitted.send(());
        let second = receive(&mut socket).await?["message"].clone();
        assert_eq!(second["requestId"], "dropped-request");
        reply(
            &mut socket,
            "test.response",
            json!({"requestId":second["requestId"],"ok":true}),
        )
        .await?;
        let _ = held.await;
        Ok::<_, anyhow::Error>(())
    });
    let client = DaemonClient::connect(options, CancellationToken::new()).await?;
    {
        let pending = client.request(
            json!({"type":"test.request","requestId":"dropped-request"}),
            "test.response",
            DEADLINE,
        );
        tokio::pin!(pending);
        tokio::select! {
            result = &mut pending => anyhow::bail!("Request unexpectedly completed: {result:?}"),
            result = observed => result?,
        }
    }
    assert_eq!(
        client
            .request(
                json!({"type":"test.request","requestId":"dropped-request"}),
                "test.response",
                DEADLINE
            )
            .await?["ok"],
        true
    );
    client.close().await;
    let _ = release.send(());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn oversized_binary_download_does_not_close_connection() -> Result<()> {
    let (listener, options, _home) = fixture().await?;
    let (release, held) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut socket = accept(&listener).await?;
        let read = receive(&mut socket).await?["message"].clone();
        let id = read["requestId"].as_str().unwrap();
        let frame = FileTransferFrame::Begin {
            request_id: id,
            metadata: FileBeginMetadata {
                mime: "application/octet-stream".into(),
                size: 4096,
                encoding: FileEncoding::Binary,
                modified_at: "2026-01-01T00:00:00Z".into(),
                revision: None,
                file_name: None,
            },
        };
        socket
            .send(Message::Binary(encode_file_transfer_frame(&frame)?.into()))
            .await?;
        let request = receive(&mut socket).await?["message"].clone();
        reply(
            &mut socket,
            "test.response",
            json!({"requestId":request["requestId"],"ok":true}),
        )
        .await?;
        let _ = held.await;
        Ok::<_, anyhow::Error>(())
    });
    let client = DaemonClient::connect(options, CancellationToken::new()).await?;
    let error = client
        .read_file("/test", "oversized.bin", 8, RequestOptions::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("byte ceiling"));
    assert_eq!(
        client
            .request(json!({"type":"test.request"}), "test.response", DEADLINE)
            .await?["ok"],
        true
    );
    client.close().await;
    let _ = release.send(());
    server.await??;
    Ok(())
}
