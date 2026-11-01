//! Cloudflare's existing relay cutover proxy, with request routing in Rust.
//!
//! This edge never accepts, decodes or terminates a WebSocket itself. Cloudflare's
//! fetch implementation forwards the upgrade and the unmodified upstream response;
//! the native paseo-relay service owns connection identity, limits and cleanup.

/// Replace only the origin, retaining the original path and query (including
/// repeated parameters). The configured upstream's path/query are intentionally
/// ignored, matching the existing relay cutover proxy.
pub fn upstream_url(request: &str, upstream: &str) -> Result<String, &'static str> {
    let mut target = url::Url::parse(request).map_err(|_| "invalid request URL")?;
    let origin = url::Url::parse(upstream).map_err(|_| "invalid relay upstream")?;
    if !matches!(origin.scheme(), "http" | "https") || origin.host_str().is_none() {
        return Err("relay upstream must be an HTTP(S) origin");
    }
    target
        .set_scheme(origin.scheme())
        .map_err(|_| "invalid upstream scheme")?;
    target
        .set_host(origin.host_str())
        .map_err(|_| "invalid upstream host")?;
    target
        .set_port(origin.port())
        .map_err(|_| "invalid upstream port")?;
    Ok(target.into())
}

#[cfg(target_arch = "wasm32")]
mod edge {
    use js_sys::{Promise, Reflect};
    use wasm_bindgen::{JsCast, prelude::*};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Request, RequestInit, Response, ResponseInit};

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_namespace = globalThis, js_name = fetch, catch)]
        fn platform_fetch(request: &Request) -> Result<Promise, JsValue>;
    }

    fn unavailable() -> Result<Response, JsValue> {
        let init = ResponseInit::new();
        init.set_status(503);
        Response::new_with_opt_str_and_init(Some("Relay upstream unavailable"), &init)
    }

    /// Worker fetch entrypoint. Headers, method, streaming body, redirect mode,
    /// and upgrade semantics come from the original Request as RequestInit.
    /// Neither encrypted frames nor authentication material are read or logged.
    #[wasm_bindgen]
    pub async fn proxy_fetch(request: Request, env: JsValue) -> Result<Response, JsValue> {
        let upstream = Reflect::get(&env, &JsValue::from_str("PASEO_RELAY_UPSTREAM"))?.as_string();
        let Some(upstream) = upstream.filter(|value| !value.is_empty()) else {
            return unavailable();
        };
        let Ok(target) = super::upstream_url(&request.url(), &upstream) else {
            return unavailable();
        };
        // The Fetch API explicitly permits a Request as the init argument. This
        // is the same clone-with-new-URL operation as new Request(url, request),
        // avoiding body buffering and preserving Cloudflare request properties.
        let outbound =
            Request::new_with_str_and_init(&target, request.unchecked_ref::<RequestInit>())?;
        match platform_fetch(&outbound) {
            Ok(promise) => match JsFuture::from(promise).await {
                Ok(response) => response.dyn_into::<Response>(),
                Err(_) => unavailable(),
            },
            Err(_) => unavailable(),
        }
    }

    /// Preserve the deployed Durable Object class/namespace without deleting its
    /// migration or storage. Existing direct namespace fetches use the same Rust
    /// upstream proxy; newly routed public requests do not use Durable Objects.
    #[wasm_bindgen]
    pub struct RelayDurableObject {
        env: JsValue,
    }

    #[wasm_bindgen]
    impl RelayDurableObject {
        #[wasm_bindgen(constructor)]
        pub fn new(_state: JsValue, env: JsValue) -> Self {
            Self { env }
        }

        pub async fn fetch(&self, request: Request) -> Result<Response, JsValue> {
            proxy_fetch(request, self.env.clone()).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::upstream_url;

    #[test]
    fn preserves_websocket_identity_and_repeated_query_parameters() {
        assert_eq!(upstream_url(
            "https://relay.paseo.sh/ws?v=2&role=client&serverId=a%2Fb&connectionId=c%20d&x=1&x=2",
            "https://paseo-relay-next.fly.dev/ignored?ignored=true",
        ).unwrap(), "https://paseo-relay-next.fly.dev/ws?v=2&role=client&serverId=a%2Fb&connectionId=c%20d&x=1&x=2");
    }

    #[test]
    fn proxies_health_and_unknown_paths_without_rewriting_them() {
        for path in ["/health", "/", "/not-found", "/a%2Fb?q=%26"] {
            assert_eq!(
                upstream_url(
                    &format!("https://relay.paseo.sh{path}"),
                    "http://127.0.0.1:8080"
                )
                .unwrap(),
                format!("http://127.0.0.1:8080{path}")
            );
        }
    }

    #[test]
    fn replaces_ports_and_supports_ipv6_upstreams() {
        assert_eq!(
            upstream_url("http://localhost:9000/ws", "https://relay.example").unwrap(),
            "https://relay.example/ws"
        );
        assert_eq!(
            upstream_url("https://relay.example/ws", "http://[::1]:8080").unwrap(),
            "http://[::1]:8080/ws"
        );
    }

    #[test]
    fn rejects_missing_or_non_http_upstream() {
        for upstream in [
            "",
            "bad url",
            "file:///etc/passwd",
            "wss://relay.example/ws",
        ] {
            assert!(upstream_url("https://relay.paseo.sh/ws", upstream).is_err());
        }
        assert!(upstream_url("invalid", "https://relay.example").is_err());
    }
}
