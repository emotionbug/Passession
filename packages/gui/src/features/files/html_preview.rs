//! Script-free HTML preview in the system browser (Flutter `html_preview.dart`): a loopback
//! HTTP server on a random port behind a random route token serves the editor's current text as
//! the document and relative workspace assets read through the daemon. GET/HEAD only; requests
//! outside the asset root are refused; every response carries a strict CSP (no scripts, frames
//! or forms), `nosniff`, `no-store` and `no-referrer`. The route is never exposed to the daemon.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use makepad_widgets::*;
use paseo_gui_core::CoreHandle;

use super::path::{Style, remote_absolute};

const CSP: &str = "default-src 'self' data:; script-src 'none'; object-src 'none'; frame-src 'none'; style-src 'self' 'unsafe-inline'; img-src 'self' data: https:; font-src 'self' data:; form-action 'none'; base-uri 'self'; sandbox allow-same-origin";

/// A started preview; `url` is known once the server thread bound its port.
pub struct HtmlPreview {
    content: Arc<Mutex<String>>,
    closed: Arc<AtomicBool>,
    port: Option<u16>,
    pub url: Option<String>,
    ready: ToUIReceiver<Result<(u16, String), String>>,
    /// Asset failures to surface (Flutter `onError`).
    errors: ToUIReceiver<String>,
    /// Open as soon as ready.
    pub open_pending: bool,
}

fn portable(windows: bool, value: &str) -> String {
    if windows { value.replace('\\', "/") } else { value.to_owned() }
}

/// POSIX `normalize` over `/`-separated paths.
fn normalize(path: &str) -> String {
    Style::Posix.normalize(path)
}

fn is_within(parent: &str, child: &str) -> bool {
    let parent = parent.trim_end_matches('/');
    child.len() > parent.len() && child.starts_with(parent) && child.as_bytes()[parent.len()] == b'/'
}

impl HtmlPreview {
    pub fn start(core: CoreHandle, host_id: &str, cwd: &str, path: &str, content: String) -> HtmlPreview {
        let ready = ToUIReceiver::default();
        let errors = ToUIReceiver::default();
        let preview = HtmlPreview {
            content: Arc::new(Mutex::new(content)),
            closed: Arc::new(AtomicBool::new(false)),
            port: None,
            url: None,
            ready,
            errors,
            open_pending: false,
        };
        let windows = Style::of(cwd) == Style::Windows;
        let root = normalize(&portable(windows, cwd));
        let absolute = normalize(&portable(windows, &remote_absolute(cwd, path)));
        // Out-of-workspace files are confined to their own directory.
        let asset_root = if is_within(&root, &absolute) { root } else { Style::Posix.dirname(&absolute) };
        let relative = Style::Posix.relative(&absolute, &asset_root);
        let token = uuid::Uuid::new_v4().to_string();
        let ready_sender = preview.ready.sender();
        let error_sender = preview.errors.sender();
        let content = preview.content.clone();
        let closed = preview.closed.clone();
        let host_id = host_id.to_owned();
        let cwd = cwd.to_owned();
        std::thread::Builder::new()
            .name("paseo-html-preview".into())
            .spawn(move || {
                let listener = match TcpListener::bind(("127.0.0.1", 0)) {
                    Ok(listener) => listener,
                    Err(error) => {
                        let _ = ready_sender.send(Err(error.to_string()));
                        return;
                    }
                };
                let port = match listener.local_addr() {
                    Ok(address) => address.port(),
                    Err(error) => {
                        let _ = ready_sender.send(Err(error.to_string()));
                        return;
                    }
                };
                let prefix = format!("/{token}/");
                let url = format!("http://127.0.0.1:{port}{prefix}{}", encode_path(&relative));
                let _ = ready_sender.send(Ok((port, url)));
                for stream in listener.incoming() {
                    if closed.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let context = Request {
                        core: core.clone(),
                        host_id: host_id.clone(),
                        cwd: cwd.clone(),
                        windows,
                        asset_root: asset_root.clone(),
                        absolute: absolute.clone(),
                        prefix: prefix.clone(),
                        content: content.clone(),
                        closed: closed.clone(),
                        errors: error_sender.clone(),
                    };
                    std::thread::Builder::new()
                        .name("paseo-html-request".into())
                        .spawn(move || context.serve(stream))
                        .ok();
                }
            })
            .ok();
        preview
    }

    pub fn update(&self, content: &str) {
        let mut current = self.content.lock().unwrap();
        if *current != content {
            *current = content.to_owned();
        }
    }

    /// Readiness and asset errors since the last poll.
    pub fn poll(&mut self) -> (bool, Vec<String>) {
        let mut became_ready = false;
        let mut errors = Vec::new();
        while let Ok(result) = self.ready.try_recv() {
            match result {
                Ok((port, url)) => {
                    self.port = Some(port);
                    self.url = Some(url);
                    became_ready = true;
                }
                Err(error) => errors.push(error),
            }
        }
        while let Ok(error) = self.errors.try_recv() {
            if !self.closed.load(Ordering::SeqCst) {
                errors.push(error);
            }
        }
        (became_ready, errors)
    }

    /// Stops the server; in-flight asset reads are dropped.
    pub fn close(&mut self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(port) = self.port {
            // Wake the blocking accept so the thread sees `closed`.
            std::thread::spawn(move || {
                let _ = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(1));
            });
        }
    }
}

impl Drop for HtmlPreview {
    fn drop(&mut self) {
        self.close();
    }
}

struct Request {
    core: CoreHandle,
    host_id: String,
    cwd: String,
    windows: bool,
    asset_root: String,
    absolute: String,
    prefix: String,
    content: Arc<Mutex<String>>,
    closed: Arc<AtomicBool>,
    errors: ToUISender<String>,
}

fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| percent_encoding::utf8_percent_encode(segment, percent_encoding::NON_ALPHANUMERIC).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn decode_path(path: &str) -> String {
    percent_encoding::percent_decode_str(path).decode_utf8_lossy().into_owned()
}

impl Request {
    fn respond(stream: &mut TcpStream, status: &str, mime: Option<&str>, body: &[u8], head: bool) {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: {CSP}\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        if let Some(mime) = mime {
            response.push_str(&format!("Content-Type: {mime}\r\n"));
        }
        response.push_str("\r\n");
        let _ = stream.write_all(response.as_bytes());
        if !head {
            let _ = stream.write_all(body);
        }
        let _ = stream.flush();
    }

    fn serve(self, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(clone) => clone,
            Err(_) => return,
        });
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        // Drain headers.
        loop {
            let mut header = String::new();
            match reader.read_line(&mut header) {
                Ok(0) | Err(_) => break,
                Ok(_) if header == "\r\n" || header == "\n" => break,
                _ => {}
            }
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let target = parts.next().unwrap_or("");
        let target_path = target.split(['?', '#']).next().unwrap_or("");
        let head = method == "HEAD";
        if !(method == "GET" || head) || !target_path.starts_with(&self.prefix) {
            Self::respond(&mut stream, "404 Not Found", None, b"", head);
            return;
        }
        let requested_relative = decode_path(&target_path[self.prefix.len()..]);
        let requested = normalize(&format!("{}/{}", self.asset_root, portable(self.windows, &requested_relative)));
        if requested != self.asset_root && !is_within(&self.asset_root, &requested) {
            Self::respond(&mut stream, "403 Forbidden", None, b"", head);
            return;
        }
        if requested == self.absolute {
            let body = self.content.lock().unwrap().clone();
            Self::respond(&mut stream, "200 OK", Some("text/html; charset=utf-8"), body.as_bytes(), head);
            return;
        }
        // A workspace asset read through the host's current connection.
        let (sender, receiver) = mpsc::channel();
        let path = if self.windows { requested.replace('/', "\\") } else { requested.clone() };
        let cwd = self.cwd.clone();
        self.core.operation(&self.host_id, move |client| async move {
            let options = paseo_client::RequestOptions { timeout: Duration::from_secs(60), ..Default::default() };
            let result = client
                .read_file(&cwd, &path, super::service::PREVIEW_MAX_BYTES, options)
                .await
                .map(|file| (file.metadata.mime, file.bytes.to_vec()))
                .map_err(|error| error.to_string());
            let failed = result.as_ref().err().cloned();
            let _ = sender.send(result);
            match failed {
                Some(error) => Err(anyhow::anyhow!(error)),
                None => Ok(serde_json::Value::Null),
            }
        });
        match receiver.recv_timeout(Duration::from_secs(90)) {
            Ok(Ok((mime, bytes))) => Self::respond(&mut stream, "200 OK", Some(&mime), &bytes, head),
            Ok(Err(error)) => {
                if !self.closed.load(Ordering::SeqCst) {
                    let _ = self.errors.send(error);
                }
                Self::respond(&mut stream, "404 Not Found", None, b"", head);
            }
            Err(_) => Self::respond(&mut stream, "404 Not Found", None, b"", head),
        }
    }
}
