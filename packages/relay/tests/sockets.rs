use futures_util::{SinkExt, StreamExt};
use paseo_relay::{
    Config, Relay,
    crypto::{Frame, KeyPair, SecureChannel, SecurityMode},
    router,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
struct Server {
    base: String,
    relay: Relay,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.relay.shutdown();
        self.task.abort();
    }
}
impl Server {
    async fn start(config: Config) -> Self {
        let relay = Relay::new(config).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("ws://{}", listener.local_addr().unwrap());
        let app = router(relay.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, relay, task }
    }
    async fn socket(&self, query: &str) -> Socket {
        connect_async(format!("{}/ws?{query}", self.base))
            .await
            .unwrap()
            .0
    }
    async fn empty(&self) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.relay.counts() != (0, 0) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("relay must release every session and socket");
    }
}
async fn receive(ws: &mut Socket) -> Message {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match ws.next().await.expect("socket ended").expect("socket read") {
                Message::Ping(_) | Message::Pong(_) => continue,
                frame => return frame,
            }
        }
    })
    .await
    .expect("socket response deadline")
}
async fn control(ws: &mut Socket) -> Value {
    serde_json::from_str(receive(ws).await.to_text().unwrap()).unwrap()
}
async fn closed(ws: &mut Socket, code: u16) {
    match receive(ws).await {
        Message::Close(Some(close)) => assert_eq!(u16::from(close.code), code),
        frame => panic!("expected close, got {frame:?}"),
    }
}
fn to_wire(frame: Frame) -> Message {
    match frame {
        Frame::Text(text) => Message::Text(text.into()),
        Frame::Binary(bytes) => Message::Binary(bytes.into()),
    }
}
fn from_wire(frame: Message) -> Frame {
    match frame {
        Message::Text(text) => Frame::Text(text.to_string()),
        Message::Binary(bytes) => Frame::Binary(bytes.to_vec()),
        frame => panic!("not data: {frame:?}"),
    }
}

#[tokio::test]
async fn v2_routes_control_and_multiple_clients_without_crossing_ids() {
    let server = Server::start(Config::default()).await;
    let mut control_ws = server.socket("v=2&role=server&serverId=daemon").await;
    assert_eq!(
        control(&mut control_ws).await,
        json!({"type":"sync","connectionIds":[]})
    );
    let mut client = server
        .socket("v=2&role=client&serverId=daemon&connectionId=a")
        .await;
    assert_eq!(
        control(&mut control_ws).await,
        json!({"type":"connected","connectionId":"a"})
    );
    let mut other = server
        .socket("v=2&role=client&serverId=daemon&connectionId=b")
        .await;
    assert_eq!(control(&mut control_ws).await["connectionId"], "b");
    let mut shared = server
        .socket("v=2&role=client&serverId=daemon&connectionId=a")
        .await;
    assert_eq!(control(&mut control_ws).await["connectionId"], "a");
    let mut data = server
        .socket("v=2&role=server&serverId=daemon&connectionId=a")
        .await;
    client
        .send(Message::Binary(vec![0, 255, 0, 18].into()))
        .await
        .unwrap();
    assert_eq!(
        receive(&mut data).await,
        Message::Binary(vec![0, 255, 0, 18].into())
    );
    data.send(Message::Text("opaque".into())).await.unwrap();
    assert_eq!(receive(&mut client).await, Message::Text("opaque".into()));
    assert_eq!(receive(&mut shared).await, Message::Text("opaque".into()));
    assert!(
        tokio::time::timeout(Duration::from_millis(40), other.next())
            .await
            .is_err()
    );
    client.close(None).await.unwrap();
    shared
        .send(Message::Text("still present".into()))
        .await
        .unwrap();
    assert_eq!(
        receive(&mut data).await,
        Message::Text("still present".into())
    );
    shared.close(None).await.unwrap();
    closed(&mut data, 1001).await;
    assert_eq!(
        control(&mut control_ws).await,
        json!({"type":"disconnected","connectionId":"a"})
    );
    other.close(None).await.unwrap();
    assert_eq!(control(&mut control_ws).await["type"], "disconnected");
    control_ws.close(None).await.unwrap();
    server.empty().await;
}

#[tokio::test]
async fn replacements_do_not_clean_up_the_new_socket() {
    let server = Server::start(Config::default()).await;
    let mut old_control = server.socket("v=2&role=server&serverId=s").await;
    control(&mut old_control).await;
    let mut client = server
        .socket("v=2&role=client&serverId=s&connectionId=a")
        .await;
    control(&mut old_control).await;
    let mut old_data = server
        .socket("v=2&role=server&serverId=s&connectionId=a")
        .await;
    let mut new_control = server.socket("v=2&role=server&serverId=s").await;
    assert_eq!(
        control(&mut new_control).await,
        json!({"type":"sync","connectionIds":["a"]})
    );
    closed(&mut old_control, 1008).await;
    let mut new_data = server
        .socket("v=2&role=server&serverId=s&connectionId=a")
        .await;
    closed(&mut old_data, 1008).await;
    client
        .send(Message::Text("after replacement".into()))
        .await
        .unwrap();
    assert_eq!(
        receive(&mut new_data).await,
        Message::Text("after replacement".into())
    );
    new_data.close(None).await.unwrap();
    closed(&mut client, 1012).await;
    assert_eq!(control(&mut new_control).await["type"], "disconnected");
    new_control.close(None).await.unwrap();
    server.empty().await;
}

#[tokio::test]
async fn buffering_keeps_newest_frames_and_enforces_bytes() {
    let server = Server::start(Config {
        pending_frames: 2,
        pending_bytes: 6,
        ..Config::default()
    })
    .await;
    let mut client = server
        .socket("v=2&role=client&serverId=s&connectionId=a")
        .await;
    for text in ["old", "two", "new"] {
        client.send(Message::Text(text.into())).await.unwrap();
    }
    // A control ping on the same socket gives an ordered protocol-level barrier
    // after all application frames have been processed by the relay.
    client.send(Message::Ping(vec![42].into())).await.unwrap();
    assert!(matches!(
        client.next().await.unwrap().unwrap(),
        Message::Pong(_)
    ));
    let mut data = server
        .socket("v=2&role=server&serverId=s&connectionId=a")
        .await;
    assert_eq!(receive(&mut data).await, Message::Text("two".into()));
    assert_eq!(receive(&mut data).await, Message::Text("new".into()));
    let mut oversized = server
        .socket("v=2&role=client&serverId=s&connectionId=b")
        .await;
    oversized
        .send(Message::Text("1234567".into()))
        .await
        .unwrap();
    closed(&mut oversized, 1009).await;
    client.close(None).await.unwrap();
    closed(&mut data, 1001).await;
    server.empty().await;
}

#[tokio::test]
async fn missing_connection_id_is_assigned_and_legacy_ping_is_answered() {
    let server = Server::start(Config::default()).await;
    let mut control_ws = server.socket("v=2&role=server&serverId=s").await;
    control(&mut control_ws).await;
    let mut client = server.socket("v=2&role=client&serverId=s").await;
    let connected = control(&mut control_ws).await;
    let id = connected["connectionId"].as_str().unwrap();
    assert!(id.starts_with("conn_"));
    let mut data = server
        .socket(&format!("v=2&role=server&serverId=s&connectionId={id}"))
        .await;
    control_ws
        .send(Message::Text("{\"type\":\"ping\"}".into()))
        .await
        .unwrap();
    let pong = control(&mut control_ws).await;
    assert_eq!(pong["type"], "pong");
    assert!(pong["ts"].as_u64().unwrap() > 0);
    data.send(Message::Text("assigned".into())).await.unwrap();
    assert_eq!(receive(&mut client).await, Message::Text("assigned".into()));
    drop(data);
    drop(client);
    drop(control_ws);
    server.empty().await;
}

#[tokio::test]
async fn legacy_replacement_and_version_isolation() {
    let server = Server::start(Config::default()).await;
    let mut daemon = server.socket("role=server&serverId=s").await;
    let mut old = server.socket("role=client&serverId=s").await;
    let mut client = server.socket("v=1&role=client&serverId=s").await;
    closed(&mut old, 1008).await;
    let mut v2 = server
        .socket("v=2&role=client&serverId=s&connectionId=a")
        .await;
    daemon
        .send(Message::Binary(vec![4, 2].into()))
        .await
        .unwrap();
    assert_eq!(
        receive(&mut client).await,
        Message::Binary(vec![4, 2].into())
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(40), v2.next())
            .await
            .is_err()
    );
    client.send(Message::Text("legacy".into())).await.unwrap();
    assert_eq!(receive(&mut daemon).await, Message::Text("legacy".into()));
    drop(daemon);
    drop(client);
    drop(v2);
    server.empty().await;
}

#[tokio::test]
async fn unresponsive_control_is_nudged_then_reset() {
    let server = Server::start(Config {
        control_nudge_after: Duration::from_millis(50),
        control_reset_after: Duration::from_millis(50),
        ..Config::default()
    })
    .await;
    let mut daemon = server.socket("v=2&role=server&serverId=s").await;
    control(&mut daemon).await;
    let client = server
        .socket("v=2&role=client&serverId=s&connectionId=a")
        .await;
    assert_eq!(control(&mut daemon).await["type"], "connected");
    assert_eq!(
        control(&mut daemon).await,
        json!({"type":"sync","connectionIds":["a"]})
    );
    closed(&mut daemon, 1011).await;
    drop(client);
    server.empty().await;
}

#[tokio::test]
async fn admission_and_frame_limits_are_real_websocket_boundaries() {
    let server = Server::start(Config {
        max_sessions: 1,
        max_sockets: 2,
        max_message_bytes: 16,
        ..Config::default()
    })
    .await;
    let mut client = server
        .socket("v=2&role=client&serverId=s&connectionId=a")
        .await;
    assert!(
        connect_async(format!("{}/ws?v=2&role=client&serverId=other", server.base))
            .await
            .is_err()
    );
    assert!(
        connect_async(format!("{}/ws?v=3&role=client&serverId=s", server.base))
            .await
            .is_err()
    );
    assert!(
        connect_async(format!("{}/ws?v=2&role=intruder&serverId=s", server.base))
            .await
            .is_err()
    );
    assert!(
        connect_async(format!("{}/ws?v=2&role=client&serverId=", server.base))
            .await
            .is_err()
    );
    client
        .send(Message::Binary(vec![0; 17].into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(frame)) = client.next().await {
            if matches!(frame, Message::Close(_)) {
                break;
            }
        }
    })
    .await
    .unwrap();
    server.empty().await;
}

#[tokio::test]
async fn protected_endpoint_handshake_and_records_pass_unchanged() {
    let server = Server::start(Config::default()).await;
    let mut client = server
        .socket("v=2&role=client&serverId=opaque&connectionId=encrypted")
        .await;
    let key = Arc::new(KeyPair::generate().unwrap());
    let mut initiator =
        SecureChannel::initiator(key.public_key(), SecurityMode::Protected).unwrap();
    let mut responder = SecureChannel::responder(key, SecurityMode::Protected);
    let hello = Message::Text(initiator.hello().unwrap().to_owned().into());
    client.send(hello.clone()).await.unwrap();
    let mut daemon = server
        .socket("v=2&role=server&serverId=opaque&connectionId=encrypted")
        .await;
    let forwarded = receive(&mut daemon).await;
    assert_eq!(forwarded, hello);
    let ready = responder
        .receive(from_wire(forwarded))
        .unwrap()
        .outbound
        .unwrap();
    daemon.send(to_wire(ready.clone())).await.unwrap();
    responder.acknowledge_ready();
    let forwarded = receive(&mut client).await;
    assert_eq!(from_wire(forwarded.clone()), ready);
    assert_eq!(
        initiator.receive(from_wire(forwarded)).unwrap().opened,
        Some(SecurityMode::Protected)
    );
    for plaintext in [
        Frame::Text("owner credential stays endpoint-only".into()),
        Frame::Binary(vec![0, 255, 31, 0]),
    ] {
        let encrypted = initiator.encode(plaintext.clone()).unwrap();
        assert_ne!(encrypted.as_bytes(), plaintext.as_bytes());
        client.send(to_wire(encrypted.clone())).await.unwrap();
        let forwarded = from_wire(receive(&mut daemon).await);
        assert_eq!(forwarded, encrypted);
        assert_eq!(
            responder.receive(forwarded).unwrap().message,
            Some(plaintext)
        );
    }
    let plaintext = Frame::Text("authenticated daemon reply".into());
    let encrypted = responder.encode(plaintext.clone()).unwrap();
    daemon.send(to_wire(encrypted.clone())).await.unwrap();
    let forwarded = from_wire(receive(&mut client).await);
    assert_eq!(forwarded, encrypted);
    assert_eq!(
        initiator.receive(forwarded).unwrap().message,
        Some(plaintext)
    );
    drop(daemon);
    drop(client);
    server.empty().await;
}

#[tokio::test]
async fn bounded_outgoing_queue_closes_instead_of_silently_dropping_frames() {
    let server = Server::start(Config {
        queue_frames: 1,
        ..Config::default()
    })
    .await;
    let mut client = server
        .socket("v=2&role=client&serverId=s&connectionId=a")
        .await;
    for text in ["one", "two"] {
        client.send(Message::Text(text.into())).await.unwrap();
    }
    client.send(Message::Ping(vec![42].into())).await.unwrap();
    assert!(matches!(
        client.next().await.unwrap().unwrap(),
        Message::Pong(_)
    ));
    // Registration flushes the pending frames before the new socket's writer
    // starts. A one-slot output queue deterministically exercises backpressure.
    let mut daemon = server
        .socket("v=2&role=server&serverId=s&connectionId=a")
        .await;
    let frame = receive(&mut daemon).await;
    let frame = if matches!(frame, Message::Text(_)) {
        receive(&mut daemon).await
    } else {
        frame
    };
    match frame {
        Message::Close(Some(close)) => assert_eq!(u16::from(close.code), 1013),
        frame => panic!("expected slow-consumer close, got {frame:?}"),
    }
    closed(&mut client, 1012).await;
    server.empty().await;
}

#[tokio::test]
async fn total_buffer_budget_is_released_after_disconnect() {
    let server = Server::start(Config {
        max_message_bytes: 16,
        queue_bytes: 16,
        total_buffer_bytes: 16,
        ..Config::default()
    })
    .await;
    let mut client = server
        .socket("v=2&role=client&serverId=s&connectionId=a")
        .await;
    client
        .send(Message::Binary(vec![7; 10].into()))
        .await
        .unwrap();
    client
        .send(Message::Binary(vec![8; 10].into()))
        .await
        .unwrap();
    closed(&mut client, 1013).await;
    server.empty().await;
    let mut replacement = server
        .socket("v=2&role=client&serverId=s&connectionId=a")
        .await;
    replacement
        .send(Message::Binary(vec![9; 10].into()))
        .await
        .unwrap();
    replacement
        .send(Message::Ping(vec![42].into()))
        .await
        .unwrap();
    assert!(matches!(
        replacement.next().await.unwrap().unwrap(),
        Message::Pong(_)
    ));
    let mut daemon = server
        .socket("v=2&role=server&serverId=s&connectionId=a")
        .await;
    assert_eq!(
        receive(&mut daemon).await,
        Message::Binary(vec![9; 10].into())
    );
    drop(daemon);
    drop(replacement);
    server.empty().await;
}

#[tokio::test]
async fn socket_replacement_is_admitted_at_capacity() {
    let server = Server::start(Config {
        max_sockets: 1,
        max_sockets_per_session: 1,
        ..Config::default()
    })
    .await;
    let mut old = server.socket("role=server&serverId=s").await;
    let new = server.socket("role=server&serverId=s").await;
    closed(&mut old, 1008).await;
    assert_eq!(server.relay.counts(), (1, 1));
    assert!(
        connect_async(format!("{}/ws?role=client&serverId=s", server.base))
            .await
            .is_err()
    );
    drop(new);
    server.empty().await;
}
