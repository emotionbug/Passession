//! In-process OpenCode server that drives real legacy and v2 sessions in tests:
//! fixed JSON routes and the live SSE event stream fed by the test.

use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::{
    body::{Frame, Incoming},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::sync::mpsc;

type Body = BoxBody<Bytes, Infallible>;

#[derive(Default)]
struct State {
    replies: HashMap<String, Value>,
    events: Option<mpsc::UnboundedSender<Value>>,
}

pub(super) struct Loopback {
    pub base_url: String,
    state: Arc<Mutex<State>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Loopback {
    /// Serves `event_path` as the SSE stream; every other route answers from
    /// `reply` (`"GET /path"` or `"GET /path?decoded=query"`), 404 for unknown GETs
    /// and `{}` for other methods.
    pub async fn start(event_path: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let base_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let state = shared.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(respond(&state, event_path, request)) }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        Self {
            base_url,
            state,
            server,
        }
    }

    pub fn reply(&self, route: &str, value: Value) {
        self.state
            .lock()
            .unwrap()
            .replies
            .insert(route.to_owned(), value);
    }

    /// Waits until the session has opened its event stream.
    pub async fn streaming(&self) {
        for _ in 0..500 {
            if self.state.lock().unwrap().events.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("OpenCode session never opened its event stream");
    }

    pub fn push(&self, event: Value) {
        let state = self.state.lock().unwrap();
        state
            .events
            .as_ref()
            .expect("event stream open")
            .send(event)
            .unwrap();
    }
}

fn respond(state: &Mutex<State>, event_path: &str, request: Request<Incoming>) -> Response<Body> {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let query = url::form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let bare = format!("{method} {path}");
    let route = if query.is_empty() {
        bare.clone()
    } else {
        format!("{bare}?{query}")
    };
    let mut state = state.lock().unwrap();
    if method == "GET" && path == event_path {
        let (sender, receiver) = mpsc::unbounded_channel::<Value>();
        state.events = Some(sender);
        let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
            let event = receiver.recv().await?;
            Some((
                Ok::<_, Infallible>(Frame::data(Bytes::from(format!("data: {event}\n\n")))),
                receiver,
            ))
        });
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(StreamBody::new(stream).boxed())
            .unwrap();
    }
    let reply = state
        .replies
        .get(&route)
        .or_else(|| state.replies.get(&bare))
        .cloned()
        .or_else(|| (method != "GET").then(|| serde_json::json!({})));
    match reply {
        Some(value) => Response::builder()
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(value.to_string())).boxed())
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::new()).boxed())
            .unwrap(),
    }
}

pub(super) fn spec() -> crate::providers::api::ProviderSpec {
    crate::providers::api::ProviderSpec {
        id: "opencode".into(),
        family: "opencode".into(),
        command: vec![],
        env: Default::default(),
        options: serde_json::json!({}),
        defaults: serde_json::json!({}),
        runtime_settings: serde_json::json!({}),
        configured_models: vec![],
    }
}

pub(super) fn context(home: &std::path::Path) -> crate::providers::api::LaunchContext {
    crate::providers::api::LaunchContext {
        agent_id: "agent".into(),
        home: home.to_owned(),
        executable: home.join("paseo"),
        env: Default::default(),
        mcp_servers: serde_json::json!({}),
        tool_catalog: Value::Null,
        tool_manifest: Value::Null,
        tool_executor: None,
        cancel: tokio_util::sync::CancellationToken::new(),
    }
}

/// Receives live sink events until one matches, returning everything received.
pub(super) async fn until(
    receiver: &mut mpsc::Receiver<Value>,
    found: impl Fn(&Value) -> bool,
) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
            .await
            .unwrap_or_else(|_| panic!("no matching live event; received {seen:?}"))
            .expect("live sink open");
        let done = found(&event);
        seen.push(event);
        if done {
            return seen;
        }
    }
}

/// `(type, messageId, text)` of a child's timeline rows among live events.
pub(super) fn child_rows(events: &[Value], child: &str) -> Vec<(String, String, String)> {
    events
        .iter()
        .filter(|e| e["type"] == "provider_subagent" && e["event"]["type"] == "timeline")
        .filter(|e| e["event"]["id"] == child)
        .map(|e| {
            let item = &e["event"]["item"];
            (
                item["type"].as_str().unwrap_or("").to_owned(),
                item["messageId"].as_str().unwrap_or("").to_owned(),
                item["text"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect()
}
