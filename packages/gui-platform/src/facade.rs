//! Blocking convenience calls for UI code that already runs its native work on
//! a worker thread (settings). Never call these on the GUI thread: they wait
//! for a result that may need the GUI event loop.

/// OS appearance and language (cheap; safe on the GUI thread, cached briefly).
pub mod appearance {
    use std::{
        sync::Mutex,
        time::{Duration, Instant},
    };

    /// Android's SP-to-DP curve, sampled at its nonlinear font-scaling knots.
    /// Display density is removed on the Java side; these are text metrics only.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct TextScale {
        dp: [f64; 10],
    }

    impl Default for TextScale {
        fn default() -> Self {
            Self { dp: Self::SP }
        }
    }

    impl TextScale {
        const SP: [f64; 10] = [0.0, 8.0, 10.0, 12.0, 14.0, 18.0, 20.0, 24.0, 30.0, 100.0];

        /// Construct from native SP-to-DP samples at 0, 8, 10, 12, 14, 18,
        /// 20, 24, 30 and 100 SP, with physical display density removed.
        pub fn from_dp(dp: [f64; 10]) -> Self {
            assert!(dp[0] == 0.0 && dp.iter().all(|value| value.is_finite()));
            assert!(dp.windows(2).all(|pair| pair[1] > pair[0]));
            Self { dp }
        }

        pub fn logical_size(&self, sp: f64) -> f64 {
            if self.dp == Self::SP {
                return sp;
            }
            let upper = Self::SP.iter().position(|knot| *knot >= sp).unwrap_or(Self::SP.len() - 1);
            if upper == 0 {
                return sp;
            }
            if sp > Self::SP[upper] {
                return sp * self.dp[upper] / Self::SP[upper];
            }
            let lower = upper - 1;
            let fraction = (sp - Self::SP[lower]) / (Self::SP[upper] - Self::SP[lower]);
            self.dp[lower] + fraction * (self.dp[upper] - self.dp[lower])
        }
    }

    #[cfg(test)]
    mod tests {
        use super::TextScale;

        #[test]
        fn system_text_scale_interpolates_native_curve_without_density() {
            let identity = TextScale::default();
            assert_eq!(identity.logical_size(13.25), 13.25);
            let linear = TextScale::from_dp(TextScale::SP.map(|sp| sp * 1.5));
            assert_eq!(linear.logical_size(13.25), 19.875);
            let nonlinear = TextScale::from_dp([0.0, 16.0, 20.0, 24.0, 28.0, 34.0, 36.0, 40.0, 46.0, 116.0]);
            assert_eq!(nonlinear.logical_size(16.0), 31.0);
            assert_eq!(nonlinear.logical_size(24.0), 40.0);
            assert_eq!(nonlinear.logical_size(200.0), 232.0);
        }
    }

    struct Cached {
        at: Instant,
        dark: bool,
        language: String,
        text_scale: TextScale,
    }

    static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

    fn read<R>(f: impl FnOnce(&Cached) -> R) -> R {
        let mut cache = CACHE.lock().unwrap();
        let stale = cache.as_ref().is_none_or(|c| c.at.elapsed() > Duration::from_secs(5));
        if stale {
            #[cfg(target_os = "android")]
            let (dark, language, text_scale) = crate::native::system_appearance();
            #[cfg(not(target_os = "android"))]
            let (dark, language, text_scale) = (
                crate::native::system_prefers_dark(),
                crate::native::system_language(),
                TextScale::default(),
            );
            *cache = Some(Cached {
                at: Instant::now(),
                dark,
                language,
                text_scale,
            });
        }
        f(cache.as_ref().unwrap())
    }

    /// Theme `auto`: the OS prefers a dark appearance.
    pub fn system_prefers_dark() -> bool {
        read(|c| c.dark)
    }

    pub fn system_text_scale() -> TextScale {
        read(|c| c.text_scale)
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
