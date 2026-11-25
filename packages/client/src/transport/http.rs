//! Bounded auxiliary host resources.
use anyhow::{Result, bail, ensure};
#[cfg(not(target_arch = "wasm32"))]
use bytes::Bytes;
#[cfg(not(target_arch = "wasm32"))]
use http_body_util::{BodyExt, Empty};
#[cfg(not(target_arch = "wasm32"))]
use hyper::{Request, header};
#[cfg(not(target_arch = "wasm32"))]
use hyper_util::rt::TokioIo;
#[cfg(not(target_arch = "wasm32"))]
use paseo_tailnet::Proxy;
use tokio_util::sync::CancellationToken;

pub struct HostHttpResponse {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
}

pub enum HttpOutcome {
    Body(HostHttpResponse),
    /// The caller must reclassify this URL's network and credential authority before fetching.
    Redirect(String),
}

/// GET without automatic redirects, proxy discovery, cookies or ambient credentials.
///
/// A browser cannot read the redirect target from a manual Fetch response, so it
/// refuses redirects even when `allow_redirect` is set.
/// A supplied proxy is exclusive: failure can never select a normal-network connection.
pub async fn get(
    url: &str,
    #[cfg(not(target_arch = "wasm32"))] proxy: Option<Proxy>,
    password: Option<&str>,
    max_bytes: usize,
    cancel: CancellationToken,
    #[cfg(not(target_arch = "wasm32"))] allow_redirect: bool,
    #[cfg(target_arch = "wasm32")] _allow_redirect: bool,
) -> Result<HttpOutcome> {
    let url = url::Url::parse(url).map_err(|_| anyhow::anyhow!("Invalid resource URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "Invalid resource URL"
    );
    ensure!(
        max_bytes > 0 && max_bytes <= 32 * 1024 * 1024,
        "Invalid resource byte limit"
    );
    #[cfg(target_arch = "wasm32")]
    return browser_get(&url, password, max_bytes, cancel).await;
    #[cfg(not(target_arch = "wasm32"))]
    native_get(url, proxy, password, max_bytes, cancel, allow_redirect).await
}

#[cfg(not(target_arch = "wasm32"))]
async fn native_get(
    url: url::Url,
    proxy: Option<Proxy>,
    password: Option<&str>,
    max_bytes: usize,
    cancel: CancellationToken,
    allow_redirect: bool,
) -> Result<HttpOutcome> {
    let work = async {
        let stream = super::network::connect(&url, proxy.as_ref()).await?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        let connection = tokio::spawn(connection);
        struct Abort(tokio::task::AbortHandle);
        impl Drop for Abort {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _connection = Abort(connection.abort_handle());
        let mut request = Request::builder()
            .method("GET")
            .uri(&url[url::Position::BeforePath..url::Position::AfterQuery])
            .header(
                header::HOST,
                &url[url::Position::BeforeHost..url::Position::AfterPort],
            )
            .header(header::CONNECTION, "close");
        if let Some(password) = password {
            ensure!(
                password.bytes().all(|b| (0x21..=0x7e).contains(&b)),
                "Invalid resource authentication"
            );
            request = request.header(header::AUTHORIZATION, format!("Bearer {password}"));
        }
        let response = sender
            .send_request(request.body(Empty::<Bytes>::new())?)
            .await?;
        if allow_redirect && matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .filter(|v| v.len() <= 8192)
                .ok_or_else(|| anyhow::anyhow!("Invalid resource redirect"))?;
            let next = url
                .join(location)
                .map_err(|_| anyhow::anyhow!("Invalid resource redirect"))?;
            return Ok(HttpOutcome::Redirect(next.to_string()));
        }
        ensure!(
            response.status().is_success(),
            "Resource returned HTTP {}; redirects are not followed",
            response.status().as_u16()
        );
        let declared = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok());
        ensure!(
            declared.is_none_or(|size| size <= max_bytes),
            "Resource exceeds the byte limit"
        );
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut body = response.into_body();
        let mut bytes = Vec::with_capacity(declared.unwrap_or(0));
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame?.into_data() {
                ensure!(
                    data.len() <= max_bytes.saturating_sub(bytes.len()),
                    "Resource exceeds the byte limit"
                );
                bytes.extend_from_slice(&data);
            }
        }
        Ok(HttpOutcome::Body(HostHttpResponse {
            bytes,
            content_type,
        }))
    };
    tokio::select! { biased;
        _ = cancel.cancelled() => bail!("Resource request cancelled"),
        _ = async { match proxy.as_ref() { Some(proxy) => proxy.invalidated().await, None => std::future::pending().await } } => bail!("Embedded Tailscale route expired"),
        result = tokio::time::timeout(std::time::Duration::from_secs(30), work) => result.map_err(|_| anyhow::anyhow!("Resource request timed out"))?,
    }
}

#[cfg(target_arch = "wasm32")]
async fn browser_get(
    url: &url::Url,
    password: Option<&str>,
    max_bytes: usize,
    cancel: CancellationToken,
) -> Result<HttpOutcome> {
    let work = async {
        let authorization;
        let header;
        let headers: &[(&str, &str)] = if let Some(password) = password {
            ensure!(
                password.bytes().all(|b| (0x21..=0x7e).contains(&b)),
                "Invalid resource authentication"
            );
            authorization = format!("Bearer {password}");
            header = [("Authorization", authorization.as_str())];
            &header
        } else {
            &[]
        };
        let (reply, received) = tokio::sync::oneshot::channel();
        let _fetch = paseo_browser::fetch(url.as_str(), headers, max_bytes, move |response| {
            let _ = reply.send(response);
        });
        let response = received.await??;
        ensure!(
            (200..300).contains(&response.status),
            "Resource returned HTTP {}; redirects are not followed",
            response.status
        );
        Ok(HttpOutcome::Body(HostHttpResponse {
            bytes: response.body,
            content_type: (!response.content_type.is_empty()).then_some(response.content_type),
        }))
    };
    tokio::select! { biased;
        _ = cancel.cancelled() => bail!("Resource request cancelled"),
        result = tokio::time::timeout(std::time::Duration::from_secs(30), work) =>
            result.map_err(|_| anyhow::anyhow!("Resource request timed out"))?,
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn fixture(response: &'static [u8]) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let count = stream.read(&mut request).await.unwrap();
            assert!(
                !String::from_utf8_lossy(&request[..count])
                    .to_ascii_lowercase()
                    .contains("authorization:")
            );
            stream.write_all(response).await.unwrap();
        });
        (format!("http://{address}/image"), task)
    }

    #[tokio::test]
    async fn tailnet_http_refuses_redirect_and_oversized_body() {
        for response in [b"HTTP/1.1 302 Found\r\nLocation: http://never-resolve.invalid/\r\nContent-Length: 0\r\n\r\n".as_slice(), b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n123456789".as_slice()] {
            let (url, task) = fixture(response).await;
            assert!(get(&url, None, None, 8, CancellationToken::new(), false).await.is_err());
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn tailnet_http_preserves_bounded_body_and_type() {
        let (url, task) =
            fixture(b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 3\r\n\r\nabc")
                .await;
        let HttpOutcome::Body(response) = get(&url, None, None, 8, CancellationToken::new(), false)
            .await
            .unwrap()
        else {
            panic!("unexpected redirect");
        };
        assert_eq!(response.bytes, b"abc");
        assert_eq!(response.content_type.as_deref(), Some("image/png"));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn tailnet_http_external_redirect_returns_for_network_reclassification() {
        let (url, task) =
            fixture(b"HTTP/1.1 302 Found\r\nLocation: /icon.png\r\nContent-Length: 0\r\n\r\n")
                .await;
        let HttpOutcome::Redirect(next) = get(&url, None, None, 8, CancellationToken::new(), true)
            .await
            .unwrap()
        else {
            panic!("expected redirect");
        };
        assert_eq!(next, url.replace("/image", "/icon.png"));
        task.await.unwrap();
    }
}
