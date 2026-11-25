//! Android WebView runs on the activity thread; Makepad owns the window coordinates
//! and receives WebView messages on its separate render/event thread.

use super::{WebViewEvent, WebViewOptions, bootstrap, parse_message, wrap_html};
use crate::android::with_activity;
use jni::{
    JNIEnv,
    objects::{JClass, JString, JValue},
    sys::{jdouble, jlong},
};
use makepad_widgets::{Cx, Rect, SignalToUI, WindowId};
use std::{
    collections::HashMap,
    sync::{
        LazyLock, Mutex, OnceLock,
        atomic::{AtomicI64, Ordering},
    },
};

static AVAILABLE: OnceLock<bool> = OnceLock::new();
static NEXT_ID: AtomicI64 = AtomicI64::new(1);
static EVENTS: LazyLock<Mutex<HashMap<jlong, Vec<WebViewEvent>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn supported() -> bool {
    if let Some(&available) = AVAILABLE.get() {
        return available;
    }
    match with_activity(|env, activity| {
        env.call_method(activity, "paseoWebViewSupported", "()Z", &[])?
            .z()
    }) {
        Ok(available) => *AVAILABLE.get_or_init(|| available),
        Err(_) => false, // A caller before activity startup must not cache this.
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Placement {
    frame: [i32; 4],
    visible: [i32; 4],
    dpi: f64,
}

fn device_rect(rect: Rect, dpi: f64) -> [i32; 4] {
    let pixel = |v: f64| (v * dpi).round() as i32;
    let x = pixel(rect.pos.x);
    let y = pixel(rect.pos.y);
    [
        x,
        y,
        pixel(rect.pos.x + rect.size.x).saturating_sub(x).max(0),
        pixel(rect.pos.y + rect.size.y).saturating_sub(y).max(0),
    ]
}

pub(super) struct WebView {
    id: jlong,
    placement: Option<Placement>,
}

impl WebView {
    pub(super) fn new(_: &mut Cx, _: WindowId, options: WebViewOptions) -> Result<Self, String> {
        if !supported() {
            return Err("Android WebView is unavailable".into());
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let html = wrap_html(&options.html, &options.theme);
        let mut script = bootstrap(&options, "(message) => PaseoBridge.postMessage(message)");
        // Android's navigation callbacks do not cover same-document fragments.
        script.push_str(
            r#"document.addEventListener("click", (event) => {
                if (event.target instanceof Element && event.target.closest("a[href], a[xlink\\:href], area[href]"))
                    event.preventDefault();
            }, true);"#,
        );
        let color = options.theme.background;
        let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as i32;
        let argb = channel(color.w) << 24
            | channel(color.x) << 16
            | channel(color.y) << 8
            | channel(color.z);
        let view = Self {
            id,
            placement: None,
        };
        EVENTS.lock().unwrap().insert(id, Vec::new());
        with_activity(|env, activity| {
            env.with_local_frame(4, |env| {
                let html = env.new_string(html)?;
                let script = env.new_string(script)?;
                env.call_method(
                    activity,
                    "paseoWebViewCreate",
                    "(JLjava/lang/String;Ljava/lang/String;I)V",
                    &[
                        JValue::Long(id),
                        JValue::Object(&html),
                        JValue::Object(&script),
                        JValue::Int(argb),
                    ],
                )?
                .v()
            })
        })
        .map_err(|error| error.to_string())?;
        Ok(view)
    }

    pub(super) fn place(&mut self, _: &mut Cx, frame: Rect, visible: Option<Rect>, dpi: f64) {
        let placement = Placement {
            frame: device_rect(frame, dpi),
            visible: visible.map_or([0; 4], |rect| device_rect(rect, dpi)),
            dpi,
        };
        if self.placement == Some(placement) {
            return;
        }
        self.placement = Some(placement);
        let [x, y, width, height] = placement.frame;
        let [left, top, clip_width, clip_height] = placement.visible;
        self.call(
            "paseoWebViewPlace",
            "(JIIIIIIIID)V",
            &[
                JValue::Long(self.id),
                JValue::Int(x),
                JValue::Int(y),
                JValue::Int(width),
                JValue::Int(height),
                JValue::Int(left),
                JValue::Int(top),
                JValue::Int(clip_width),
                JValue::Int(clip_height),
                JValue::Double(dpi),
            ],
        );
    }

    pub(super) fn run_script(&mut self, script: &str) {
        let result = with_activity(|env, activity| {
            env.with_local_frame(2, |env| {
                let script = env.new_string(script)?;
                env.call_method(
                    activity,
                    "paseoWebViewEvaluate",
                    "(JLjava/lang/String;)V",
                    &[JValue::Long(self.id), JValue::Object(&script)],
                )?
                .v()
            })
        });
        if let Err(error) = result {
            queue(self.id, WebViewEvent::Failed(error.to_string()));
        }
    }

    pub(super) fn blur(&mut self) {
        self.call("paseoWebViewBlur", "(J)V", &[JValue::Long(self.id)]);
    }

    pub(super) fn pump(&mut self, _: &mut Cx) -> Vec<WebViewEvent> {
        std::mem::take(EVENTS.lock().unwrap().get_mut(&self.id).unwrap())
    }

    fn call(&self, method: &str, signature: &str, args: &[JValue]) {
        if let Err(error) =
            with_activity(|env, activity| env.call_method(activity, method, signature, args)?.v())
        {
            queue(self.id, WebViewEvent::Failed(error.to_string()));
        }
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        EVENTS.lock().unwrap().remove(&self.id);
        self.call("paseoWebViewDestroy", "(J)V", &[JValue::Long(self.id)]);
    }
}

fn queue(id: jlong, event: WebViewEvent) {
    // Detaching/dropping the view can race callbacks already en route from Java.
    if let Some(events) = EVENTS.lock().unwrap().get_mut(&id) {
        events.push(event);
        SignalToUI::set_ui_signal();
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_sh_paseo_sessions_PaseoWebViews_onMessage(
    mut env: JNIEnv,
    _: JClass,
    id: jlong,
    message: JString,
) {
    if let Ok(message) = env.get_string(&message) {
        let text: String = message.into();
        if let Some(event) = parse_message(&text) {
            queue(id, event);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_sh_paseo_sessions_PaseoWebViews_onScroll(
    _: JNIEnv,
    _: JClass,
    id: jlong,
    dy: jdouble,
) {
    queue(id, WebViewEvent::Scroll { dx: 0.0, dy });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_sh_paseo_sessions_PaseoWebViews_onFailed(
    mut env: JNIEnv,
    _: JClass,
    id: jlong,
    message: JString,
) {
    if let Ok(message) = env.get_string(&message) {
        queue(id, WebViewEvent::Failed(message.into()));
    }
}
