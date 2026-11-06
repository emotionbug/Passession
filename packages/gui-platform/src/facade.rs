//! Blocking convenience calls for UI code that already runs its native work on
//! a worker thread (settings). Never call these on the GUI thread: they wait
//! for a result that may need the GUI event loop.

/// OS appearance and language (cheap; safe on the GUI thread, cached briefly).
pub mod appearance {
    use std::{
        sync::Mutex,
        time::{Duration, Instant},
    };

    struct Cached {
        at: Instant,
        dark: bool,
        language: String,
    }

    static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

    fn read<R>(f: impl FnOnce(&Cached) -> R) -> R {
        let mut cache = CACHE.lock().unwrap();
        let stale = cache.as_ref().is_none_or(|c| c.at.elapsed() > Duration::from_secs(5));
        if stale {
            *cache = Some(Cached {
                at: Instant::now(),
                dark: crate::native::system_prefers_dark(),
                language: crate::native::system_language(),
            });
        }
        f(cache.as_ref().unwrap())
    }

    /// Theme `auto`: the OS prefers a dark appearance.
    pub fn system_prefers_dark() -> bool {
        read(|c| c.dark)
    }

    #[cfg(target_os = "android")]
    pub(crate) fn invalidate() {
        *CACHE.lock().unwrap() = None;
    }

    /// Match Android system bar icons and backgrounds to the resolved app palette.
    /// The background is an opaque ARGB color; window layout/insets stay unchanged.
    #[cfg(target_os = "android")]
    pub fn set_system_bars(dark: bool, background: u32) {
        crate::native::set_system_bars(dark, background);
    }

    /// BCP 47-ish language tag of the OS UI language (`ko-KR`, `en_US.UTF-8`, …).
    pub fn system_language() -> String {
        read(|c| c.language.clone())
    }
}

/// OS notifications.
pub mod notifications {
    use crate::{action::PlatformValue, notification_policy::NotificationRequest, platform::blocking};

    /// Asks for notification permission; `true` when allowed.
    pub fn request_permission() -> Result<bool, String> {
        match blocking(crate::native::request_notification_permission) {
            Ok(PlatformValue::Permission(state)) => Ok(state.allowed()),
            Ok(PlatformValue::Bool(allowed)) => Ok(allowed),
            Ok(_) => Ok(false),
            Err(error) => Err(error.message),
        }
    }

    /// Shows one notification (settings diagnostics "test notification").
    pub fn notify(id: i32, title: &str, body: &str) -> Result<(), String> {
        let request = NotificationRequest {
            id,
            title: title.to_owned(),
            body: Some(body.to_owned()),
            data: Default::default(),
            event_key: None,
            sound: None,
        };
        blocking(|shared, done| crate::native::notify(shared, request, done))
            .map(|_| ())
            .map_err(|error| error.message)
    }
}

/// The desktop host (built-in daemon, settings, updates). Mobile clients never
/// manage a local daemon.
pub mod desktop {
    use serde_json::Value;

    pub fn available() -> bool {
        cfg!(not(any(target_os = "android", target_os = "ios"))) && crate::platform().is_some()
    }

    /// One desktop host command (`desktop_settings_get`, `start_desktop_daemon`, …).
    pub fn invoke(command: &str, args: &Value) -> Result<Value, String> {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            let command = command.to_owned();
            let args = args.clone();
            match crate::platform::blocking(move |shared, done| crate::desktop::invoke_command(shared, command, args, done)) {
                Ok(crate::action::PlatformValue::Json(value)) => Ok(value),
                Ok(_) => Ok(Value::Null),
                Err(error) => Err(error.message),
            }
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            let _ = (command, args);
            Err("Mobile clients never manage a local daemon.".to_owned())
        }
    }
}

/// Settings diagnostics playback test.
pub(crate) fn play_thinking_tone() -> Result<(), String> {
    use crate::{action::PlatformValue, audio};
    let handle = crate::platform().ok_or_else(|| "The platform layer has not started.".to_owned())?;
    let shared = handle.0.clone();
    let id = shared.next_id();
    let receiver = crate::action::wait_for(id);
    // Original `handlePlaybackTest`: stop current output, then play to completion.
    // Both run on the GUI thread so the cancel cannot drop the new tone.
    let submit = shared.clone();
    shared.on_main(move |cx, state| {
        state.audio.cancel(cx);
        crate::platform::submit_audio(&submit, id, audio::THINKING_TONE_PCM16_16K.to_vec(), audio::THINKING_TONE_MIME.to_owned());
    });
    match receiver.recv() {
        Ok(Ok(PlatformValue::Bool(true))) => Ok(()),
        Ok(Ok(_)) => Err("Playback was interrupted.".to_owned()),
        Ok(Err(error)) => Err(error.message),
        Err(_) => Err("The playback request was dropped.".to_owned()),
    }
}
