// Starts the Passession web client: instantiates `app.wasm` (wasm32-wasip1) with Makepad's
// `env` bridge, a browser WASI preview1 and the `paseo` imports of `packages/browser/src/lib.rs`
// (both files change together), then hands the instance to Makepad's WebGL front end.
import {WasmWebGL} from "./makepad_platform/web_gl.js";
import {init_env} from "./makepad_wasm_bridge/wasm_bridge.js";

const STORAGE_PREFIX = "passession:";
const encoder = new TextEncoder();
const decoder = new TextDecoder();
let instance = null;
const canvas = document.querySelector("canvas.full_canvas");

const memory = () => instance.exports.memory;
const view = () => new DataView(memory().buffer);
const bytes = (ptr, len) => new Uint8Array(memory().buffer, ptr, len);
const text = (ptr, len) => decoder.decode(bytes(ptr, len));

// Copies `data` into a `paseo_alloc` buffer; the receiving export takes ownership.
function give(data) {
    if (data.length === 0) return 0;
    const ptr = instance.exports.paseo_alloc(data.length);
    bytes(ptr, data.length).set(data);
    return ptr;
}

// A value for an import returning `(ptr << 32) | len`, or -1 when absent.
function packed(value) {
    if (value === null) return -1n;
    const data = encoder.encode(value);
    return (BigInt(give(data)) << 32n) | BigInt(data.length);
}

const EBADF = 8;
const ESPIPE = 70;
const ENOSYS = 52;

// WASI preview1 for a page: clocks, entropy and console output. There are no files,
// arguments or environment, and nothing blocks.
function wasi() {
    const lines = {1: "", 2: ""};
    const empty = (count, size) => {
        view().setUint32(count, 0, true);
        view().setUint32(size, 0, true);
        return 0;
    };
    const calls = {
        args_sizes_get: empty,
        args_get: () => 0,
        environ_sizes_get: empty,
        environ_get: () => 0,
        clock_time_get(id, _precision, out) {
            const ns = id === 0
                ? BigInt(Date.now()) * 1000000n
                : BigInt(Math.round(performance.now() * 1e6));
            view().setBigUint64(out, ns, true);
            return 0;
        },
        random_get(ptr, len) {
            for (let at = 0; at < len; at += 65536) {
                crypto.getRandomValues(bytes(ptr + at, Math.min(65536, len - at)));
            }
            return 0;
        },
        fd_write(fd, iovs, count, written) {
            let total = 0;
            for (let i = 0; i < count; i++) {
                const ptr = view().getUint32(iovs + i * 8, true);
                const len = view().getUint32(iovs + i * 8 + 4, true);
                lines[fd] = (lines[fd] ?? "") + text(ptr, len);
                total += len;
            }
            const parts = lines[fd].split("\n");
            lines[fd] = parts.pop();
            for (const line of parts) (fd === 2 ? console.error : console.log)(line);
            view().setUint32(written, total, true);
            return 0;
        },
        // Every subscription reports ready at once: the page's thread must never block.
        poll_oneoff(subscriptions, events, count, ready) {
            for (let i = 0; i < count; i++) {
                const subscription = subscriptions + i * 48;
                const event = events + i * 32;
                view().setBigUint64(event, view().getBigUint64(subscription, true), true);
                view().setUint16(event + 8, 0, true);
                view().setUint8(event + 10, view().getUint8(subscription + 8));
            }
            view().setUint32(ready, count, true);
            return 0;
        },
        proc_exit(code) {
            throw new Error(`Passession exited with ${code}`);
        },
        sched_yield: () => 0,
        // The page has no files: no preopened directories (EBADF ends libc's scan),
        // and descriptors other than stdin/stdout/stderr do not exist.
        fd_prestat_get: () => EBADF,
        fd_prestat_dir_name: () => EBADF,
        fd_seek: () => ESPIPE,
    };
    // Every other file call (`fd_*`, `path_*`) names a descriptor that cannot exist;
    // an unforeseen import answers ENOSYS and is reported once.
    const reported = new Set();
    return new Proxy(calls, {
        get(target, name) {
            if (name in target) return target[name];
            if (typeof name !== "string") return undefined;
            if (name.startsWith("fd_") || name.startsWith("path_")) return () => EBADF;
            return () => {
                if (!reported.has(name)) {
                    reported.add(name);
                    console.warn(`WASI ${name} is not available in the browser`);
                }
                return ENOSYS;
            };
        },
    });
}

const sockets = new Map();
const fetches = new Map();
const webViews = new Map();
let webViewsAvailable;
let initForm;

function webViewsSupported() {
    return webViewsAvailable ??= !!canvas && typeof ResizeObserver === "function"
        && "sandbox" in HTMLIFrameElement.prototype && "srcdoc" in HTMLIFrameElement.prototype;
}

function blurWebView({frame}) {
    if (document.activeElement !== frame) return;
    const input = document.querySelector("textarea.cx_webgl_textinput");
    if (!input) canvas.tabIndex = -1;
    (input ?? canvas).focus({preventScroll: true});
}

function openWebView(id, page, html, bridge, config) {
    if (!webViewsSupported()) return 0;
    const clip = document.createElement("div");
    const frame = document.createElement("iframe");
    const view = {clip, frame, bridge: null};
    try {
        initForm ??= new Function("window", `${bridge}\nreturn window.paseoInit;`)({});
        // Parse elsewhere, inertly. A form may not install a refresh, a nested browsing
        // context or a base URL. Adopt the nodes after the trusted srcdoc's CSP is active;
        // do not serialize them back into HTML and reparse them in the live document.
        const template = document.createElement("template");
        template.innerHTML = html;
        for (const node of template.content.querySelectorAll(
            "base, meta, link, iframe, frame, frameset, object, embed, portal, fencedframe",
        )) node.remove();
        for (const link of template.content.querySelectorAll("a, area")) {
            link.removeAttribute("href");
            link.removeAttribute("xlink:href");
        }

        // In particular, a run_script import must return before its post reenters WASM.
        const send = (message, failed = 0) => queueMicrotask(() => {
            if (webViews.get(id) !== view) return;
            const data = encoder.encode(message);
            instance.exports.paseo_webview_event(id, give(data), data.length, failed);
        });
        clip.style.cssText = "position:fixed;overflow:hidden;overflow:clip;visibility:hidden;z-index:1";
        frame.style.cssText = "position:absolute;border:0;display:block;transform-origin:0 0;visibility:hidden";
        frame.title = "Form";
        frame.sandbox = "allow-same-origin";
        frame.onload = () => {
            try {
                const doc = frame.contentDocument;
                if (!doc || doc.URL !== "about:srcdoc" || view.bridge) {
                    throw new Error("The form iframe left its initial document");
                }
                doc.addEventListener("contextmenu", (event) => event.preventDefault());
                const guardLink = (event) => {
                    if (event.target.closest?.("a, area")) event.preventDefault();
                };
                doc.addEventListener("auxclick", guardLink, true);
                doc.addEventListener("dragstart", guardLink, true);
                doc.addEventListener("click", (event) => {
                    guardLink(event);
                    // Without allow-forms the browser suppresses the submit event. A user's
                    // activation of a submit button still goes through the bridge's validation.
                    const button = event.target.closest?.("button, input");
                    if (button?.form && button.form === doc.querySelector("form")
                        && (button.type === "submit" || button.type === "image")) {
                        event.preventDefault();
                        view.bridge?.submit();
                    }
                }, true);
                doc.body.replaceChildren(template.content);
                view.bridge = initForm(doc, (message) => send(JSON.stringify(message)), config);
                frame.style.removeProperty("visibility");
            } catch (error) {
                frame.style.visibility = "hidden";
                send(String(error?.message ?? error), 1);
            }
        };
        frame.srcdoc = page;
        clip.append(frame);
        document.body.append(clip);
        webViews.set(id, view);
        return 1;
    } catch (error) {
        clip.remove();
        console.warn("Could not open form iframe", error);
        return 0;
    }
}

function placeWebView(id, x, y, width, height, clipX, clipY, clipWidth, clipHeight, dpi) {
    const view = webViews.get(id);
    if (!view) return;
    const {clip, frame} = view;
    if (!(clipWidth > 0 && clipHeight > 0)) {
        blurWebView(view);
        clip.style.visibility = "hidden";
        return;
    }
    const bounds = canvas.getBoundingClientRect();
    // Makepad measures the canvas in CSS px and sets its backing buffer to CSS px * DPR.
    // Only a Makepad DPI override needs scaling; the iframe still lays out in logical px.
    const scale = dpi / window.devicePixelRatio;
    Object.assign(clip.style, {
        left: `${bounds.left + clipX * scale}px`,
        top: `${bounds.top + clipY * scale}px`,
        width: `${clipWidth * scale}px`,
        height: `${clipHeight * scale}px`,
        visibility: "visible",
    });
    Object.assign(frame.style, {
        left: `${(x - clipX) * scale}px`,
        top: `${(y - clipY) * scale}px`,
        width: `${width}px`,
        height: `${height}px`,
        transform: scale === 1 ? "" : `scale(${scale})`,
    });
}

function socketEvent(id, kind, data = null, code = 0) {
    const ptr = data ? give(data) : 0;
    instance.exports.paseo_ws_event(id, kind, ptr, data ? data.length : 0, code);
}

// 1 when the open socket `id` queued `frame()`, 0 when it is not open or the browser refused it.
function send(id, frame) {
    const socket = sockets.get(id);
    if (socket?.readyState !== WebSocket.OPEN) return 0;
    try {
        socket.send(frame());
        return 1;
    } catch {
        return 0;
    }
}

const paseo = {
    ws_open(id, ptr, len) {
        let socket;
        try {
            socket = new WebSocket(text(ptr, len));
        } catch (error) {
            // Invalid URL, mixed content or CSP: report like a failed connection, after this call.
            console.warn("WebSocket refused", error);
            queueMicrotask(() => {
                socketEvent(id, 3);
                socketEvent(id, 4, null, 1006);
            });
            return;
        }
        socket.binaryType = "arraybuffer";
        sockets.set(id, socket);
        socket.onopen = () => socketEvent(id, 0);
        socket.onmessage = (message) => {
            if (typeof message.data === "string") socketEvent(id, 1, encoder.encode(message.data));
            else socketEvent(id, 2, new Uint8Array(message.data));
        };
        socket.onerror = () => socketEvent(id, 3);
        socket.onclose = (close) => {
            if (sockets.get(id) !== socket) return;
            sockets.delete(id);
            socketEvent(id, 4, null, close.code);
        };
    },
    ws_send_text(id, ptr, len) {
        return send(id, () => text(ptr, len));
    },
    ws_send_binary(id, ptr, len) {
        return send(id, () => bytes(ptr, len).slice());
    },
    ws_buffered_amount: (id) => sockets.get(id)?.bufferedAmount ?? 0,
    ws_close(id) {
        const socket = sockets.get(id);
        sockets.delete(id);
        socket?.close(1000);
    },
    webview_supported: () => (webViewsSupported() ? 1 : 0),
    webview_open(id, page, pageLen, html, htmlLen, bridge, bridgeLen, config, configLen) {
        return openWebView(
            id, text(page, pageLen), text(html, htmlLen), text(bridge, bridgeLen),
            JSON.parse(text(config, configLen)),
        );
    },
    webview_place: placeWebView,
    webview_run_script(id, ptr, len) {
        const view = webViews.get(id);
        if (!view?.bridge) return;
        try {
            new Function("window", "document", text(ptr, len))(
                {paseo: view.bridge}, view.frame.contentDocument,
            );
        } catch (error) {
            console.warn("Could not run form bridge code", error);
        }
    },
    webview_blur(id) {
        const view = webViews.get(id);
        if (view) blurWebView(view);
    },
    webview_close(id) {
        const view = webViews.get(id);
        if (!view) return;
        webViews.delete(id);
        blurWebView(view);
        view.frame.onload = null;
        view.clip.remove();
    },
    storage_get(ptr, len) {
        try {
            return packed(localStorage.getItem(STORAGE_PREFIX + text(ptr, len)));
        } catch {
            return -1n;
        }
    },
    storage_set(key_ptr, key_len, value_ptr, value_len) {
        try {
            localStorage.setItem(STORAGE_PREFIX + text(key_ptr, key_len), text(value_ptr, value_len));
            return 1;
        } catch {
            return 0;
        }
    },
    storage_remove(ptr, len) {
        try {
            localStorage.removeItem(STORAGE_PREFIX + text(ptr, len));
            return 1;
        } catch {
            return 0;
        }
    },
    storage_keys() {
        try {
            const keys = [];
            for (let i = 0; i < localStorage.length; i++) {
                const key = localStorage.key(i);
                if (key?.startsWith(STORAGE_PREFIX)) keys.push(key.slice(STORAGE_PREFIX.length));
            }
            return packed(keys.join("\0"));
        } catch {
            return -1n;
        }
    },
    page_url: () => packed(location.href),
    fetch(id, url_ptr, url_len, headers_ptr, headers_len, max_bytes) {
        const url = text(url_ptr, url_len);
        const headers = JSON.parse(text(headers_ptr, headers_len));
        const controller = new AbortController();
        fetches.set(id, controller);
        // Settles once: an aborted fetch reports nothing (its Rust handle is gone).
        const settle = () => fetches.get(id) === controller && fetches.delete(id);
        // kind 0 network (with a message), 1 too large, 2 redirect.
        const failed = (kind, message = "") => {
            if (!settle()) return;
            const data = encoder.encode(message);
            instance.exports.paseo_fetch_failed(id, kind, give(data), data.length);
        };
        globalThis.fetch(url, {headers, credentials: "omit", redirect: "manual", signal: controller.signal})
            .then(async (response) => {
                if (response.type === "opaqueredirect" || (response.status >= 300 && response.status < 400)) {
                    return failed(2);
                }
                if (!response.ok) {
                    // A failure status answers at once, without its (possibly large) body.
                    response.body?.cancel().catch(() => {});
                    if (!settle()) return;
                    const kind = encoder.encode(response.headers.get("content-type") ?? "");
                    return instance.exports.paseo_fetch_done(id, response.status, 0, 0, give(kind), kind.length);
                }
                const chunks = [];
                let size = 0;
                const reader = response.body?.getReader();
                while (reader) {
                    const {done, value} = await reader.read();
                    if (done) break;
                    size += value.length;
                    if (size > max_bytes) {
                        await reader.cancel();
                        return failed(1);
                    }
                    chunks.push(value);
                }
                const body = new Uint8Array(size);
                let at = 0;
                for (const chunk of chunks) {
                    body.set(chunk, at);
                    at += chunk.length;
                }
                if (!settle()) return;
                const kind = encoder.encode(response.headers.get("content-type") ?? "");
                instance.exports.paseo_fetch_done(id, response.status, give(body), body.length, give(kind), kind.length);
            })
            .catch((error) => failed(0, String(error?.message ?? error)));
    },
    fetch_abort(id) {
        const controller = fetches.get(id);
        fetches.delete(id);
        controller?.abort();
    },
    clipboard_write(ptr, len) {
        navigator.clipboard?.writeText(text(ptr, len)).catch((error) => console.warn("Clipboard write refused", error));
    },
    open_url(ptr, len) {
        window.open(text(ptr, len), "_blank", "noopener");
    },
    download(name_ptr, name_len, mime_ptr, mime_len, data_ptr, data_len) {
        const blob = new Blob([bytes(data_ptr, data_len).slice()], {type: text(mime_ptr, mime_len) || "application/octet-stream"});
        const link = document.createElement("a");
        link.href = URL.createObjectURL(blob);
        link.download = text(name_ptr, name_len);
        link.click();
        setTimeout(() => URL.revokeObjectURL(link.href), 60000);
    },
    pick_files(id, accept_ptr, accept_len, multiple) {
        const input = document.createElement("input");
        input.type = "file";
        input.accept = text(accept_ptr, accept_len);
        input.multiple = multiple === 1;
        let settled = false;
        const finish = async () => {
            if (settled) return;
            settled = true;
            try {
                for (const file of input.files ?? []) {
                    const name = encoder.encode(file.name);
                    const mime = encoder.encode(file.type);
                    const data = new Uint8Array(await file.arrayBuffer());
                    instance.exports.paseo_pick_file(id, give(name), name.length, give(mime), mime.length, give(data), data.length);
                }
            } catch (error) {
                const message = encoder.encode(String(error?.message ?? error));
                instance.exports.paseo_pick_failed(id, give(message), message.length);
                return;
            }
            instance.exports.paseo_pick_done(id);
        };
        input.onchange = finish;
        input.oncancel = finish;
        input.click();
    },
    prefers_dark: () => (matchMedia("(prefers-color-scheme: dark)").matches ? 1 : 0),
    language: () => packed(navigator.language ?? "en"),
    timezone: () => packed(Intl.DateTimeFormat().resolvedOptions().timeZone ?? "UTC"),
    timezone_offset_minutes: (epoch_ms) => -new Date(epoch_ms).getTimezoneOffset(),
};

const module = await WebAssembly.compileStreaming(fetch("./app.wasm"));
// `log` is libghostty-vt's diagnostic output (freestanding Zig build).
const env = {_post_signal: () => {}, log: (ptr, len) => console.warn(text(ptr, len))};
const attach = init_env(env);
instance = await WebAssembly.instantiate(module, {env, wasi_snapshot_preview1: wasi(), paseo});
attach(instance);
instance._has_thread_support = false;
instance._memory = instance.exports.memory;
instance._module = module;
new WasmWebGL(instance, {}, canvas);
