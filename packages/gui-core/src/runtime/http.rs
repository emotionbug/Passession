//! Auxiliary image/icon resources share host routing and cancellation authority.
use super::{Core, tailnet};
use crate::{CoreError, HostHttpResponse, RequestId, host_config};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;
use std::rc::Rc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

type Reply = oneshot::Sender<Result<HostHttpResponse, CoreError>>;

#[allow(clippy::too_many_arguments)]
pub(super) fn request(
    core: &Rc<Core>,
    host_id: String,
    request_id: RequestId,
    url: String,
    max_bytes: usize,
    reply: Option<Reply>,
    cancel: CancellationToken,
) {
    if core.requests.borrow().contains_key(&request_id) {
        let error = CoreError::new(
            "REQUEST_ID_IN_USE",
            "Resource request id is already active",
            false,
        );
        if let Some(reply) = reply {
            let _ = reply.send(Err(error));
        } else {
            core.respond(&host_id, &request_id, Err(error));
        }
        return;
    }
    core.requests
        .borrow_mut()
        .insert(request_id.clone(), cancel.clone());
    // Byte-oriented icon loads may follow links from arbitrary external HTML. Their callers
    // must never acquire daemon credentials by choosing a same-origin candidate.
    let allow_credentials = reply.is_none();
    let core = core.clone();
    tokio::task::spawn_local(async move {
        let result = async {
            let host = core.host_for(&host_id).ok_or_else(|| crate::error::host_unknown(&host_id))?;
            let config = host.config.borrow().clone();
            let epoch = host.client_epoch.get();
            let host_cancel = host.resource_cancellation();
            let (initial_origin, initial_tailnet) = host_config::http_route(&config, &url)?;
            let external_chain = !initial_origin && !initial_tailnet;
            let transfer = async {
                let mut current = url;
                for redirects in 0..=10 {
                    let (same_origin, tailnet_host) = host_config::http_route(&config, &current)?;
                    let (proxy, network_cancel, password) = if same_origin || tailnet_host {
                        let resolved = host_config::resolve(&config)?;
                        let (proxy, cancel) = tailnet::route(&core, &config)?;
                        let credentials = allow_credentials && same_origin && !external_chain && redirects == 0;
                        (proxy, cancel, credentials.then_some(resolved.password).flatten())
                    } else {
                        (None, core.cancel.clone(), None)
                    };
                    let result = tokio::select! { biased;
                        _ = core.cancel.cancelled() => Err(cancelled()),
                        _ = cancel.cancelled() => Err(cancelled()),
                        _ = host_cancel.cancelled() => Err(cancelled()),
                        _ = network_cancel.cancelled() => Err(cancelled()),
                        result = paseo_client::transport::http::get(&current, proxy, password.as_deref(), max_bytes, cancel.clone(), external_chain && !tailnet_host) => result.map_err(|error| CoreError::from_anyhow(&error)),
                    }?;
                    if host.client_epoch.get() != epoch || host_cancel.is_cancelled() || network_cancel.is_cancelled() || !core.contains(&host) {
                        return Err(cancelled());
                    }
                    match result {
                        paseo_client::transport::http::HttpOutcome::Body(body) => return Ok(body),
                        paseo_client::transport::http::HttpOutcome::Redirect(next) => current = next,
                    }
                }
                Err(CoreError::new("HOST_HTTP_REDIRECT", "Resource exceeded ten redirects", false))
            };
            tokio::time::timeout(std::time::Duration::from_secs(30), transfer).await
                .map_err(|_| CoreError::new("HOST_HTTP_TIMEOUT", "Resource request timed out", false))?
        }.await;
        core.requests.borrow_mut().remove(&request_id);
        if let Some(reply) = reply {
            let _ = reply.send(result);
        } else {
            let result = result.map(|body| json!({"type":"host.http.response","payload":{
                "requestId":request_id,"bodyBase64":STANDARD.encode(body.bytes),"contentType":body.content_type
            }}));
            core.respond(&host_id, &request_id, result);
        }
    });
}

fn cancelled() -> CoreError {
    CoreError::new(
        "HOST_HTTP_CANCELLED",
        "Resource request cancelled because the host or network changed",
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn core(path: &std::path::Path, endpoint: String) -> Rc<Core> {
        let core = Core::new(
            crate::CoreConfig {
                state_dir: path.to_owned(),
                daemon_home: None,
                default_endpoint: None,
                tailnet_key_store: None,
            },
            Arc::new(crate::events::EventQueue::new(Arc::new(|| {}))),
        );
        let config =
            json!({"id":"host","endpoint":endpoint,"password":"never-send-from-external-content"});
        core.hosts.borrow_mut().push(super::super::hosts::Host::new(
            1,
            config.clone(),
            "host".into(),
            None,
            vec![config],
            None,
        ));
        core
    }

    async fn no_auth_server(listener: TcpListener) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            request.push(stream.read_u8().await.unwrap());
            if request.ends_with(b"\r\n\r\n") {
                break;
            }
            assert!(request.len() < 8192);
        }
        assert!(
            !String::from_utf8_lossy(&request)
                .to_ascii_lowercase()
                .contains("authorization:")
        );
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn tailnet_http_icon_candidates_never_acquire_daemon_bearer() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let server = tokio::spawn(no_auth_server(listener));
                let dir = tempfile::tempdir().unwrap();
                let core = core(dir.path(), format!("ws://{address}/ws"));
                let (reply, result) = oneshot::channel();
                request(
                    &core,
                    "host".into(),
                    "icon".into(),
                    format!("http://{address}/icon"),
                    8,
                    Some(reply),
                    CancellationToken::new(),
                );
                let response = tokio::time::timeout(std::time::Duration::from_secs(5), result)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                assert_eq!(response.bytes, b"ok");
                server.await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn tailnet_http_public_redirect_into_daemon_never_acquires_bearer() {
        tokio::task::LocalSet::new().run_until(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let daemon = listener.local_addr().unwrap();
            let server = tokio::spawn(no_auth_server(listener));
            let public = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let public_port = public.local_addr().unwrap().port();
            let redirect = tokio::spawn(async move {
                let (mut stream, _) = public.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    assert!(request.len() < 8192);
                    request.push(stream.read_u8().await.unwrap());
                }
                stream.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://{daemon}/image\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
            });
            let dir = tempfile::tempdir().unwrap();
            let core = core(dir.path(), format!("ws://{daemon}/ws"));
            request(&core, "host".into(), "image".into(), format!("http://localhost:{public_port}/public"), 8, None, CancellationToken::new());
            let response = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let mut events = Vec::new();
                    while core.queue.stage(&mut events) {}
                    for event in events {
                        if let crate::CoreEvent::Response { result, .. } = event { return result.unwrap(); }
                    }
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
            assert_eq!(response["payload"]["bodyBase64"], "b2s=");
            redirect.await.unwrap();
            server.await.unwrap();
        }).await;
    }
}
