//! Browser bindings of the web client, built for `wasm32-wasip1`.
//!
//! The page's `web/boot.js` instantiates the module with the `paseo` imports below and
//! calls the `paseo_*` exports; the two files change together. Everything here runs on
//! the page's single thread. Native builds compile this crate empty.
#![cfg(target_arch = "wasm32")]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

mod sys {
    #[link(wasm_import_module = "paseo")]
    unsafe extern "C" {
        pub fn ws_open(id: u32, url: *const u8, url_len: usize);
        /// 1 when queued, 0 when the socket is not open (or the browser refused the frame).
        pub fn ws_send_text(id: u32, text: *const u8, len: usize) -> u32;
        pub fn ws_send_binary(id: u32, data: *const u8, len: usize) -> u32;
        /// Bytes queued by `send` that the browser has not yet sent (`WebSocket.bufferedAmount`).
        pub fn ws_buffered_amount(id: u32) -> usize;
        pub fn ws_close(id: u32);
        /// `(ptr << 32) | len` of a buffer from [`super::paseo_alloc`], or -1 when absent or
        /// when the browser refused access.
        pub fn storage_get(key: *const u8, key_len: usize) -> i64;
        /// 1 when stored, 0 when the browser refused (quota, private mode).
        pub fn storage_set(
            key: *const u8,
            key_len: usize,
            value: *const u8,
            value_len: usize,
        ) -> u32;
        /// 1 when removed (or absent), 0 when the browser refused access.
        pub fn storage_remove(key: *const u8, key_len: usize) -> u32;
        /// This app's keys (prefix removed) joined with NUL, packed like `storage_get`.
        pub fn storage_keys() -> i64;
        /// `location.href` as [`storage_get`] returns a value.
        pub fn page_url() -> i64;
        /// GET `url` with `headers` (a JSON object), no cookies, no redirects followed, reading at
        /// most `max_bytes`; answered by `paseo_fetch_done` or `paseo_fetch_failed`.
        pub fn fetch(
            id: u32,
            url: *const u8,
            url_len: usize,
            headers: *const u8,
            headers_len: usize,
            max_bytes: usize,
        );
        /// Aborts fetch `id`; it then reports nothing.
        pub fn fetch_abort(id: u32);
        pub fn clipboard_write(text: *const u8, len: usize);
        pub fn open_url(url: *const u8, len: usize);
        /// Saves `data` as a download named `name`.
        pub fn download(
            name: *const u8,
            name_len: usize,
            mime: *const u8,
            mime_len: usize,
            data: *const u8,
            data_len: usize,
        );
        /// Opens the file chooser; answered by `paseo_pick_file` per file, then `paseo_pick_done(id)`,
        /// or `paseo_pick_failed(id, …)` when reading a chosen file failed.
        pub fn pick_files(id: u32, accept: *const u8, accept_len: usize, multiple: u32);
        /// 1 when the page prefers a dark color scheme.
        pub fn prefers_dark() -> u32;
        /// `navigator.language`, packed like [`storage_get`].
        pub fn language() -> i64;
        /// The IANA time zone (`Intl.DateTimeFormat().resolvedOptions().timeZone`), packed.
        pub fn timezone() -> i64;
        /// Minutes east of UTC of the page's time zone at `epoch_ms`.
        pub fn timezone_offset_minutes(epoch_ms: f64) -> i32;
    }
}

/// A buffer of `len` bytes for the page to fill. The page hands it back with the call that
/// names it, which takes ownership.
#[unsafe(no_mangle)]
pub extern "C" fn paseo_alloc(len: usize) -> *mut u8 {
    Box::into_raw(vec![0u8; len].into_boxed_slice()).cast()
}

/// Takes back a buffer from [`paseo_alloc`]; a zero length carries no buffer.
unsafe fn take(ptr: *mut u8, len: usize) -> Vec<u8> {
    if len == 0 || ptr.is_null() {
        return Vec::new();
    }
    unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) }.into_vec()
}

/// Unpacks a `(ptr << 32) | len` result; negative means absent.
unsafe fn take_packed(packed: i64) -> Option<Vec<u8>> {
    if packed < 0 {
        return None;
    }
    Some(unsafe {
        take(
            (packed >> 32) as usize as *mut u8,
            (packed & 0xffff_ffff) as usize,
        )
    })
}

thread_local! {
    static WAKE: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
    static SOCKETS: RefCell<HashMap<u32, Box<dyn FnMut(SocketEvent)>>> = RefCell::new(HashMap::new());
    static NEXT_SOCKET: Cell<u32> = const { Cell::new(1) };
    /// The socket whose handler is running, and whether that handler dropped it.
    static DISPATCHING: Cell<Option<(u32, bool)>> = const { Cell::new(None) };
}

/// Called after every browser event this crate delivers, so the app processes what the
/// event's handler queued (the GUI installs Makepad's UI signal).
pub fn set_wake(wake: impl Fn() + 'static) {
    WAKE.with(|slot| *slot.borrow_mut() = Some(Box::new(wake)));
}

fn wake() {
    WAKE.with(|slot| {
        if let Some(wake) = slot.borrow().as_ref() {
            wake();
        }
    });
}

/// What a browser WebSocket reports, in order. `Failed` is followed by `Closed`.
#[derive(Debug)]
pub enum SocketEvent {
    Opened,
    Text(String),
    Binary(Vec<u8>),
    Failed,
    Closed { code: u16 },
}

/// A browser WebSocket. Dropping it closes the socket without a further event.
pub struct Socket {
    id: u32,
}

impl Socket {
    /// Opens `url` (`ws:` or `wss:`); `on_event` receives every event until `Closed`.
    pub fn open(url: &str, on_event: impl FnMut(SocketEvent) + 'static) -> Socket {
        let id = NEXT_SOCKET.with(|next| {
            let id = next.get();
            next.set(id.wrapping_add(1).max(1));
            id
        });
        SOCKETS.with(|sockets| sockets.borrow_mut().insert(id, Box::new(on_event)));
        unsafe { sys::ws_open(id, url.as_ptr(), url.len()) };
        Socket { id }
    }

    /// Queues a text frame; false when the socket is not open or the browser refused it.
    #[must_use]
    pub fn send_text(&self, text: &str) -> bool {
        unsafe { sys::ws_send_text(self.id, text.as_ptr(), text.len()) == 1 }
    }

    /// Queues a binary frame; false when the socket is not open or the browser refused it.
    #[must_use]
    pub fn send_binary(&self, data: &[u8]) -> bool {
        unsafe { sys::ws_send_binary(self.id, data.as_ptr(), data.len()) == 1 }
    }

    /// Bytes queued by sends that the browser has not handed to the network yet. The browser
    /// reports no event when it drains, so callers poll this.
    pub fn buffered_amount(&self) -> usize {
        unsafe { sys::ws_buffered_amount(self.id) }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        DISPATCHING.with(|current| {
            if let Some((id, _)) = current.get()
                && id == self.id
            {
                current.set(Some((id, true)));
            }
        });
        SOCKETS.with(|sockets| sockets.borrow_mut().remove(&self.id));
        unsafe { sys::ws_close(self.id) };
    }
}

/// Socket `id` reported `kind` (0 opened, 1 text, 2 binary, 3 failed, 4 closed with `code`).
/// `data`/`len` is a [`paseo_alloc`] buffer this call owns.
#[unsafe(no_mangle)]
pub extern "C" fn paseo_ws_event(id: u32, kind: u32, data: *mut u8, len: usize, code: u32) {
    let data = unsafe { take(data, len) };
    let event = match kind {
        0 => SocketEvent::Opened,
        1 => SocketEvent::Text(String::from_utf8_lossy(&data).into_owned()),
        2 => SocketEvent::Binary(data),
        3 => SocketEvent::Failed,
        _ => SocketEvent::Closed { code: code as u16 },
    };
    let closed = matches!(event, SocketEvent::Closed { .. });
    // The handler runs outside the map's borrow: it may open or drop sockets.
    let Some(mut handler) = SOCKETS.with(|sockets| sockets.borrow_mut().remove(&id)) else {
        return;
    };
    DISPATCHING.with(|current| current.set(Some((id, false))));
    handler(event);
    let dropped = DISPATCHING
        .with(|current| current.take())
        .is_some_and(|(_, dropped)| dropped);
    if !closed && !dropped {
        SOCKETS.with(|sockets| sockets.borrow_mut().insert(id, handler));
    }
    wake();
}

/// The page's `localStorage` value of `key`; `None` when absent or when the browser refused access.
pub fn storage_get(key: &str) -> Option<String> {
    let value = unsafe { take_packed(sys::storage_get(key.as_ptr(), key.len())) }?;
    String::from_utf8(value).ok()
}

/// Stores `value` under `key`; false when the browser refused it (quota, private mode).
pub fn storage_set(key: &str, value: &str) -> bool {
    unsafe { sys::storage_set(key.as_ptr(), key.len(), value.as_ptr(), value.len()) == 1 }
}

/// Removes `key`; false when the browser refused access (an absent key counts as removed).
pub fn storage_remove(key: &str) -> bool {
    unsafe { sys::storage_remove(key.as_ptr(), key.len()) == 1 }
}

/// Every key this app stored, as passed to [`storage_set`]; empty when the browser refused access.
pub fn storage_keys() -> Vec<String> {
    packed_string(unsafe { sys::storage_keys() })
        .split('\0')
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The page's `location.href`.
pub fn page_url() -> String {
    packed_string(unsafe { sys::page_url() })
}

fn packed_string(packed: i64) -> String {
    unsafe { take_packed(packed) }
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

thread_local! {
    static NEXT_REQUEST: Cell<u32> = const { Cell::new(1) };
    static FETCHES: RefCell<HashMap<u32, Box<dyn FnOnce(Result<FetchResponse, FetchError>)>>> =
        RefCell::new(HashMap::new());
    static PICKS: RefCell<HashMap<u32, (Vec<PickedFile>, PickDone)>> = RefCell::new(HashMap::new());
}

fn next_request() -> u32 {
    NEXT_REQUEST.with(|next| {
        let id = next.get();
        next.set(id.wrapping_add(1).max(1));
        id
    })
}

/// A completed page `fetch`.
#[derive(Debug)]
pub struct FetchResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Why a [`fetch`] produced no response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The browser's network or CORS failure, with its message.
    Network(String),
    /// The body grew past `max_bytes`; reading stopped there.
    TooLarge,
    /// The server answered with a redirect; the page cannot read its target, so it is not followed.
    Redirect,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(message) => write!(f, "{message}"),
            Self::TooLarge => f.write_str("Response is too large"),
            Self::Redirect => f.write_str("Redirects are not followed"),
        }
    }
}

impl std::error::Error for FetchError {}

/// GETs `url` (same-origin or CORS-permitted) with exactly `headers`: no cookies are sent and
/// redirects are not followed. A 2xx body is read up to `max_bytes`; a longer one fails with
/// [`FetchError::TooLarge`]. Any other status answers at once with an empty body. Dropping the
/// returned [`Fetch`] before `done` runs aborts the request; `done` then never runs.
pub fn fetch(
    url: &str,
    headers: &[(&str, &str)],
    max_bytes: usize,
    done: impl FnOnce(Result<FetchResponse, FetchError>) + 'static,
) -> Fetch {
    let id = next_request();
    FETCHES.with(|fetches| fetches.borrow_mut().insert(id, Box::new(done)));
    let headers = json_object(headers);
    unsafe {
        sys::fetch(
            id,
            url.as_ptr(),
            url.len(),
            headers.as_ptr(),
            headers.len(),
            max_bytes,
        )
    };
    Fetch { id }
}

/// A [`fetch`] in flight; dropping it aborts the request.
#[must_use = "dropping the handle aborts the fetch"]
pub struct Fetch {
    id: u32,
}

impl Drop for Fetch {
    fn drop(&mut self) {
        if FETCHES
            .with(|fetches| fetches.borrow_mut().remove(&self.id))
            .is_some()
        {
            unsafe { sys::fetch_abort(self.id) };
        }
    }
}

fn finish_fetch(id: u32, result: Result<FetchResponse, FetchError>) {
    let Some(done) = FETCHES.with(|fetches| fetches.borrow_mut().remove(&id)) else {
        return;
    };
    done(result);
    wake();
}

/// Answers [`fetch`] `id` with a response; every buffer is owned by this call.
#[unsafe(no_mangle)]
pub extern "C" fn paseo_fetch_done(
    id: u32,
    status: u32,
    body: *mut u8,
    body_len: usize,
    content_type: *mut u8,
    content_type_len: usize,
) {
    let body = unsafe { take(body, body_len) };
    let content_type = unsafe { take(content_type, content_type_len) };
    finish_fetch(
        id,
        Ok(FetchResponse {
            status: status as u16,
            content_type: String::from_utf8_lossy(&content_type).into_owned(),
            body,
        }),
    );
}

/// Fails [`fetch`] `id`: `kind` 0 network (with `message`), 1 too large, 2 redirect.
#[unsafe(no_mangle)]
pub extern "C" fn paseo_fetch_failed(id: u32, kind: u32, message: *mut u8, message_len: usize) {
    let message = unsafe { take(message, message_len) };
    finish_fetch(
        id,
        Err(match kind {
            1 => FetchError::TooLarge,
            2 => FetchError::Redirect,
            _ => FetchError::Network(String::from_utf8_lossy(&message).into_owned()),
        }),
    );
}

/// `{"name":"value",…}` with JSON string escaping.
fn json_object(pairs: &[(&str, &str)]) -> String {
    fn quote(out: &mut String, text: &str) {
        out.push('"');
        for ch in text.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    let mut out = String::from("{");
    for (index, (name, value)) in pairs.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        quote(&mut out, name);
        out.push(':');
        quote(&mut out, value);
    }
    out.push('}');
    out
}

/// Copies `text` to the system clipboard (asynchronous; the browser may refuse without focus).
pub fn clipboard_write(text: &str) {
    unsafe { sys::clipboard_write(text.as_ptr(), text.len()) };
}

/// Opens `url` in a new browser tab.
pub fn open_url(url: &str) {
    unsafe { sys::open_url(url.as_ptr(), url.len()) };
}

/// Hands `data` to the browser as a download named `name`.
pub fn download(name: &str, mime: &str, data: &[u8]) {
    unsafe {
        sys::download(
            name.as_ptr(),
            name.len(),
            mime.as_ptr(),
            mime.len(),
            data.as_ptr(),
            data.len(),
        )
    };
}

/// A file the user chose in the browser's file chooser.
#[derive(Debug, Clone)]
pub struct PickedFile {
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
}

type PickDone = Box<dyn FnOnce(Result<Vec<PickedFile>, String>)>;

/// Opens the browser's file chooser (`accept` as for `<input accept>`, empty for any).
/// `done` receives the chosen files (none when the user cancelled), or the browser's error when
/// a chosen file could not be read.
pub fn pick_files(
    accept: &str,
    multiple: bool,
    done: impl FnOnce(Result<Vec<PickedFile>, String>) + 'static,
) {
    let id = next_request();
    PICKS.with(|picks| picks.borrow_mut().insert(id, (Vec::new(), Box::new(done))));
    unsafe { sys::pick_files(id, accept.as_ptr(), accept.len(), u32::from(multiple)) };
}

/// One file of [`pick_files`] `id`; every argument buffer is owned by this call.
#[unsafe(no_mangle)]
pub extern "C" fn paseo_pick_file(
    id: u32,
    name: *mut u8,
    name_len: usize,
    mime: *mut u8,
    mime_len: usize,
    data: *mut u8,
    data_len: usize,
) {
    let file = unsafe {
        PickedFile {
            name: String::from_utf8_lossy(&take(name, name_len)).into_owned(),
            mime: String::from_utf8_lossy(&take(mime, mime_len)).into_owned(),
            data: take(data, data_len),
        }
    };
    PICKS.with(|picks| {
        if let Some((files, _)) = picks.borrow_mut().get_mut(&id) {
            files.push(file);
        }
    });
}

fn finish_pick(id: u32, result: impl FnOnce(Vec<PickedFile>) -> Result<Vec<PickedFile>, String>) {
    let Some((files, done)) = PICKS.with(|picks| picks.borrow_mut().remove(&id)) else {
        return;
    };
    done(result(files));
    wake();
}

/// Ends [`pick_files`] `id` with the files it reported.
#[unsafe(no_mangle)]
pub extern "C" fn paseo_pick_done(id: u32) {
    finish_pick(id, Ok);
}

/// Fails [`pick_files`] `id`: reading a chosen file failed with `message` (owned by this call).
#[unsafe(no_mangle)]
pub extern "C" fn paseo_pick_failed(id: u32, message: *mut u8, message_len: usize) {
    let message = String::from_utf8_lossy(&unsafe { take(message, message_len) }).into_owned();
    finish_pick(id, |_| Err(message));
}

/// Whether the page prefers a dark color scheme.
pub fn prefers_dark() -> bool {
    unsafe { sys::prefers_dark() == 1 }
}

/// The browser's preferred language tag (`navigator.language`).
pub fn language() -> String {
    packed_string(unsafe { sys::language() })
}

/// The page's IANA time zone name.
pub fn timezone() -> String {
    packed_string(unsafe { sys::timezone() })
}

/// Minutes east of UTC of the page's time zone at `epoch_ms` (Unix milliseconds).
pub fn timezone_offset_minutes(epoch_ms: f64) -> i32 {
    unsafe { sys::timezone_offset_minutes(epoch_ms) }
}
