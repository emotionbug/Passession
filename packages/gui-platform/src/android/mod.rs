//! Android bridge: Rust calls methods on `sh.paseo.sessions.MainActivity`
//! (the Makepad activity subclass) through JNI; Java reports asynchronous
//! results and OS events back through `sh.paseo.sessions.PaseoBridge`'s native
//! methods. Payloads are small JSON documents; bulk data (picked files,
//! clipboard images) travels as private cache files.

pub(crate) mod native;

use crate::{
    action::{post_event, post_result, PlatformError, PlatformEvent, PlatformRequestId, PlatformValue},
    mobile::{deliver, MobileSignal},
    platform::Completion,
};
use jni::{
    objects::{JClass, JObject, JString, JValue},
    sys::jlong,
    JNIEnv, JavaVM,
};
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    sync::{atomic::AtomicI64, LazyLock, Mutex},
};

type Convert = fn(Value) -> Result<PlatformValue, PlatformError>;

enum Pending {
    Completion(Completion, Convert),
    Channel(tokio::sync::oneshot::Sender<Result<Value, PlatformError>>),
}

static PENDING: LazyLock<Mutex<HashMap<i64, Pending>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// Channel requests use negative ids so they never collide with request ids.
static NEXT_CHANNEL: AtomicI64 = AtomicI64::new(-1);

fn vm() -> Result<JavaVM, PlatformError> {
    let raw = makepad_android_state::get_java_vm();
    if raw.is_null() {
        return Err(PlatformError::native("The Java VM is not available yet."));
    }
    unsafe { JavaVM::from_raw(raw as *mut jni::sys::JavaVM) }.map_err(|e| PlatformError::native(e.to_string()))
}

fn with_activity<R>(f: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<R>) -> Result<R, PlatformError> {
    let vm = vm()?;
    let mut env = vm.attach_current_thread_permanently().map_err(|e| PlatformError::native(e.to_string()))?;
    let raw = makepad_android_state::get_activity();
    if raw.is_null() {
        return Err(PlatformError::native("The Android activity is not available."));
    }
    // A global reference owned by Makepad; never deleted here.
    let activity = unsafe { JObject::from_raw(raw as jni::sys::jobject) };
    let result = f(&mut env, &activity);
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    result.map_err(|e| PlatformError::native(format!("Android bridge: {e}")))
}

fn parse_reply(text: &str) -> Result<Value, PlatformError> {
    let reply: Value = serde_json::from_str(text).map_err(|e| PlatformError::native(format!("Invalid bridge reply: {e}")))?;
    if let Some(error) = reply.get("error") {
        return Err(PlatformError::new(
            error.get("code").and_then(Value::as_str).unwrap_or("native"),
            error.get("message").and_then(Value::as_str).unwrap_or("Android operation failed."),
        ));
    }
    Ok(reply.get("ok").cloned().unwrap_or(Value::Null))
}

/// `MainActivity.paseoCall(method, argsJson) -> replyJson` (runs synchronously
/// on the calling thread; Java posts UI work to its main thread itself).
pub(crate) fn call(method: &str, args: Value) -> Result<Value, PlatformError> {
    let reply = with_activity(|env, activity| {
        let method = env.new_string(method)?;
        let args = env.new_string(args.to_string())?;
        let value = env
            .call_method(
                activity,
                "paseoCall",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(&method), JValue::Object(&args)],
            )?
            .l()?;
        let text: String = env.get_string(&JString::from(value))?.into();
        Ok(text)
    })?;
    parse_reply(&reply)
}

fn call_async_raw(id: i64, method: &str, args: Value) -> Result<(), PlatformError> {
    with_activity(|env, activity| {
        let method = env.new_string(method)?;
        let args = env.new_string(args.to_string())?;
        env.call_method(
            activity,
            "paseoCallAsync",
            "(JLjava/lang/String;Ljava/lang/String;)V",
            &[JValue::Long(id), JValue::Object(&method), JValue::Object(&args)],
        )?;
        Ok(())
    })
}

/// Asynchronous Java operation finishing `done` through `nativeResult`.
pub(crate) fn call_async(method: &str, args: Value, done: Completion, convert: Convert) {
    let id = done.id() as i64;
    PENDING.lock().unwrap().insert(id, Pending::Completion(done, convert));
    if let Err(error) = call_async_raw(id, method, args) {
        if let Some(Pending::Completion(done, _)) = PENDING.lock().unwrap().remove(&id) {
            done.fail(error);
        }
    }
}

/// Asynchronous Java operation awaited from Rust.
pub(crate) async fn call_await(method: &str, args: Value) -> Result<Value, PlatformError> {
    let id = NEXT_CHANNEL.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = tokio::sync::oneshot::channel();
    PENDING.lock().unwrap().insert(id, Pending::Channel(tx));
    if let Err(error) = call_async_raw(id, method, args) {
        PENDING.lock().unwrap().remove(&id);
        return Err(error);
    }
    rx.await.unwrap_or_else(|_| Err(PlatformError::native("The Android operation was dropped.")))
}

/// `MainActivity.paseoAudioPlay(request, pcm16, channels, rate)`: queued on the
/// Java AudioTrack player, completed through `nativeResult(request, …)`.
pub(crate) fn audio_play(request: PlatformRequestId, pcm16: &[u8], channels: i32, rate: i32) -> Result<(), PlatformError> {
    with_activity(|env, activity| {
        let bytes = env.byte_array_from_slice(pcm16)?;
        env.call_method(
            activity,
            "paseoAudioPlay",
            "(J[BII)V",
            &[JValue::Long(request as i64), JValue::Object(&bytes), JValue::Int(channels), JValue::Int(rate)],
        )?;
        Ok(())
    })
}

pub(crate) fn audio_cancel() {
    let _ = with_activity(|env, activity| {
        env.call_method(activity, "paseoAudioCancel", "()V", &[])?;
        Ok(())
    });
}

fn finish(id: i64, result: Result<Value, PlatformError>) {
    let pending = PENDING.lock().unwrap().remove(&id);
    match pending {
        Some(Pending::Completion(done, convert)) => done.finish(result.and_then(convert)),
        Some(Pending::Channel(tx)) => {
            let _ = tx.send(result);
        }
        // Audio completions are keyed by platform request ids directly.
        None if id > 0 => post_result(id as PlatformRequestId, result.map(|v| PlatformValue::Bool(v.as_bool().unwrap_or(false)))),
        None => {}
    }
}

fn jstring(env: &mut JNIEnv, value: &JString) -> String {
    env.get_string(value).map(Into::into).unwrap_or_default()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_sh_paseo_sessions_PaseoBridge_nativeResult(mut env: JNIEnv, _class: JClass, id: jlong, reply: JString) {
    let reply = jstring(&mut env, &reply);
    finish(id, parse_reply(&reply));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_sh_paseo_sessions_PaseoBridge_nativeEvent(mut env: JNIEnv, _class: JClass, kind: JString, payload: JString) {
    let kind = jstring(&mut env, &kind);
    let payload: Value = serde_json::from_str(&jstring(&mut env, &payload)).unwrap_or(Value::Null);
    let text = |key: &str| payload.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    match kind.as_str() {
        "link" => deliver(MobileSignal::Link(text("url"))),
        "notificationTap" => {
            let data = payload.get("data").and_then(Value::as_object).cloned().unwrap_or_default();
            deliver(MobileSignal::NotificationTap(text("id"), crate::mobile::notification_data(&data)));
        }
        "resumed" | "appearanceChanged" => {
            crate::appearance::invalidate();
            post_event(PlatformEvent::AppearanceChanged);
            if kind == "resumed" {
                deliver(MobileSignal::Resumed);
            }
        }
        "insets" => deliver(MobileSignal::Insets(native::insets_from_json(&payload))),
        "audioInterrupted" => deliver(MobileSignal::AudioInterrupted),
        "tokenRefreshed" => deliver(MobileSignal::TokenRefreshed),
        _ => {}
    }
}

pub(crate) fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}
