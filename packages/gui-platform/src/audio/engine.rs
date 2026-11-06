//! One shared FIFO playback engine on Makepad's audio output (original shared
//! FlutterAudioEngine). Sources play in submission order; each request
//! completes when its source has finished (`Bool(true)`) or was cancelled
//! (`Bool(false)`). The output device is opened only while something plays.

use super::DecodedAudio;
use crate::action::{post_result, PlatformError, PlatformRequestId, PlatformValue};
use makepad_widgets::{AudioDeviceId, AudioDevicesEvent, Cx, CxMediaApi};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct Playing {
    request: PlatformRequestId,
    audio: DecodedAudio,
    frame: usize,
}

/// A platform player that takes over decoded sources in FIFO order (Android
/// AudioTrack). It completes each request itself.
#[derive(Clone)]
pub(crate) struct AudioSink {
    pub play: Arc<dyn Fn(PlatformRequestId, DecodedAudio) + Send + Sync>,
    pub cancel: Arc<dyn Fn() + Send + Sync>,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Playing>,
    /// Requests admitted but still decoding, in submission order.
    pending: VecDeque<(PlatformRequestId, Option<DecodedAudio>)>,
    generation: u64,
    sink: Option<AudioSink>,
}

#[derive(Default)]
pub(crate) struct AudioEngine {
    state: Arc<Mutex<State>>,
    callback_installed: bool,
    default_output: Vec<AudioDeviceId>,
    device_open: bool,
    session_held: bool,
}

/// Cloneable submission side used from worker threads.
#[derive(Clone, Default)]
pub(crate) struct AudioQueue {
    state: Arc<Mutex<State>>,
}

impl AudioEngine {
    pub fn queue(&self) -> AudioQueue {
        AudioQueue { state: self.state.clone() }
    }

    /// Routes playback to a native player instead of Makepad's output device.
    #[cfg(target_os = "android")]
    pub fn set_sink(&mut self, sink: AudioSink) {
        self.state.lock().unwrap().sink = Some(sink);
    }

    /// GUI thread: keeps the device open exactly while sources are queued.
    pub fn update(&mut self, cx: &mut Cx) {
        let wants = {
            let state = self.state.lock().unwrap();
            if state.sink.is_some() {
                // The native player owns its device and audio focus.
                return;
            }
            !state.queue.is_empty() || !state.pending.is_empty()
        };
        if wants && !self.callback_installed {
            self.install_callback(cx);
        }
        if wants && !self.session_held {
            match crate::native::acquire_audio_session() {
                Ok(()) => self.session_held = true,
                Err(error) => {
                    // Focus denied: nothing queued may play (original failPlayback).
                    self.cancel_with(Err(PlatformError::new("audio_focus", error.to_string())));
                    return;
                }
            }
        }
        if wants && !self.device_open && !self.default_output.is_empty() {
            cx.use_audio_outputs(&self.default_output);
            self.device_open = true;
        }
        if !wants && self.device_open {
            cx.use_audio_outputs(&[]);
            self.device_open = false;
        }
        if !wants && self.session_held {
            crate::native::release_audio_session();
            self.session_held = false;
        }
    }

    pub fn devices(&mut self, cx: &mut Cx, event: &AudioDevicesEvent) {
        let output = event.default_output();
        if output != self.default_output {
            if self.device_open {
                cx.use_audio_outputs(&[]);
                self.device_open = false;
            }
            self.default_output = output;
        }
        self.update(cx);
    }

    /// The OS took the audio session (focus loss / interruption): drop everything.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub fn interrupted(&mut self, cx: &mut Cx) {
        use crate::action::{post_event, PlatformEvent};
        self.cancel_with(Ok(PlatformValue::Bool(false)));
        post_event(PlatformEvent::AudioInterrupted);
        self.update(cx);
    }

    pub fn cancel(&mut self, cx: &mut Cx) {
        self.cancel_with(Ok(PlatformValue::Bool(false)));
        self.update(cx);
    }

    fn cancel_with(&mut self, result: Result<PlatformValue, PlatformError>) {
        let (playing, pending, sink) = {
            let mut state = self.state.lock().unwrap();
            state.generation = state.generation.wrapping_add(1);
            (std::mem::take(&mut state.queue), std::mem::take(&mut state.pending), state.sink.clone())
        };
        if let Some(sink) = sink {
            // Completes what the native player holds with Bool(false).
            (sink.cancel)();
        }
        for item in playing {
            post_result(item.request, result.clone());
        }
        for (request, _) in pending {
            post_result(request, result.clone());
        }
    }

    fn install_callback(&mut self, cx: &mut Cx) {
        let state = self.state.clone();
        cx.audio_output(0, move |_info, buffer| {
            buffer.zero();
            let frames = buffer.frame_count();
            let channels = buffer.channel_count();
            let mut finished = Vec::new();
            {
                let mut state = state.lock().unwrap();
                let mut written = 0;
                while written < frames {
                    let Some(current) = state.queue.front_mut() else { break };
                    let source_channels = current.audio.channels.max(1);
                    let total = current.audio.samples.len() / source_channels;
                    let available = total.saturating_sub(current.frame);
                    let count = available.min(frames - written);
                    for c in 0..channels {
                        let source_channel = c.min(source_channels - 1);
                        let out = buffer.channel_mut(c);
                        for i in 0..count {
                            out[written + i] = current.audio.samples[(current.frame + i) * source_channels + source_channel];
                        }
                    }
                    current.frame += count;
                    written += count;
                    if current.frame >= total
                        && let Some(done) = state.queue.pop_front() {
                            finished.push(done.request);
                        }
                }
            }
            for request in finished {
                post_result(request, Ok(PlatformValue::Bool(true)));
                makepad_widgets::SignalToUI::set_ui_signal();
            }
        });
        self.callback_installed = true;
    }
}

impl AudioQueue {
    /// Reserves the request's place in the FIFO before decoding.
    pub fn admit(&self, request: PlatformRequestId) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.pending.push_back((request, None));
        state.generation
    }

    /// Decoding finished; sources move to the play queue in admission order.
    pub fn decoded(&self, request: PlatformRequestId, generation: u64, result: anyhow::Result<DecodedAudio>) {
        let mut ready_failures = Vec::new();
        {
            let mut state = self.state.lock().unwrap();
            if state.generation != generation {
                return; // cancelled while decoding; already completed
            }
            match result {
                Ok(audio) => {
                    if let Some(slot) = state.pending.iter_mut().find(|(id, _)| *id == request) {
                        slot.1 = Some(audio);
                    }
                }
                Err(error) => {
                    state.pending.retain(|(id, _)| *id != request);
                    ready_failures.push(PlatformError::new("audio", error.to_string()));
                }
            }
            let mut handed = Vec::new();
            while state.pending.front().is_some_and(|(_, audio)| audio.is_some()) {
                let (id, audio) = state.pending.pop_front().unwrap();
                match &state.sink {
                    Some(sink) => handed.push((sink.clone(), id, audio.unwrap())),
                    None => state.queue.push_back(Playing { request: id, audio: audio.unwrap(), frame: 0 }),
                }
            }
            // Still under the lock, so hand-off order equals admission order.
            for (sink, id, audio) in handed {
                (sink.play)(id, audio);
            }
        }
        for error in ready_failures {
            post_result(request, Err(error));
        }
        makepad_widgets::SignalToUI::set_ui_signal();
    }
}
