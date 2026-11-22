//! The GUI-thread `Platform` owner and its cloneable `PlatformHandle`.

use crate::{
    action::*,
    audio::{self, engine::AudioEngine},
    native,
    notification_policy::NotificationRequest,
};
use makepad_widgets::{Cx, Event, SignalToUI};
use paseo_gui_core::{CoreEvent, CoreHandle};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

pub struct PlatformConfig {
    pub app_version: String,
    pub argv: Vec<String>,
    pub core: CoreHandle,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub desktop_host: bool,
    pub editors: bool,
    pub updater: bool,
    pub directory_picker: bool,
    pub save_dialog: bool,
    pub push: bool,
    pub qr_scan: bool,
    pub share: bool,
    /// Images can be saved straight into the device's photo library ([`Platform::save_image`]).
    pub photo_library: bool,
    pub idle: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PickFilesOptions {
    pub multiple: bool,
    pub images_only: bool,
    pub title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OpenEditorRequest {
    pub editor_id: String,
    pub workspace_path: PathBuf,
    pub file_path: Option<PathBuf>,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WindowGeometry {
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub width: f64,
    pub height: f64,
    pub maximized: bool,
}

/// GUI-thread work submitted from other threads.
pub(crate) type MainTask = Box<dyn FnOnce(&mut Cx, &mut PlatformState) + Send>;

/// A single completion for one request; native code finishes it exactly once.
#[must_use]
pub(crate) struct Completion(PlatformRequestId);

impl Completion {
    pub fn id(&self) -> PlatformRequestId {
        self.0
    }
    pub fn finish(self, result: Result<PlatformValue, PlatformError>) {
        post_result(self.0, result);
    }
    pub fn ok(self, value: PlatformValue) {
        self.finish(Ok(value));
    }
    pub fn fail(self, error: PlatformError) {
        self.finish(Err(error));
    }
}

pub(crate) struct Shared {
    next_request: AtomicU64,
    pub runtime: tokio::runtime::Runtime,
    main_tasks: Mutex<Vec<MainTask>>,
    pub audio: audio::engine::AudioQueue,
    pub insets: Mutex<SafeInsets>,
    /// Mobile push registration sends through the core's host connections.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub core: CoreHandle,
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub desktop: crate::desktop::DesktopShared,
    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub mobile: crate::mobile::MobileShared,
}

impl Shared {
    pub fn next_id(&self) -> PlatformRequestId {
        self.next_request.fetch_add(1, Ordering::Relaxed)
    }
    pub fn completion(&self) -> Completion {
        Completion(self.next_id())
    }
    /// Runs on the GUI thread at the next event (wakes the UI).
    pub fn on_main(&self, task: impl FnOnce(&mut Cx, &mut PlatformState) + Send + 'static) {
        self.main_tasks.lock().unwrap().push(Box::new(task));
        SignalToUI::set_ui_signal();
    }
    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.runtime.spawn(future);
    }
    pub fn spawn_blocking<F>(&self, work: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.runtime.spawn_blocking(work);
    }
    /// Finishes `done` with the result of blocking `work` on a worker thread.
    #[cfg(not(target_os = "ios"))]
    pub fn complete_blocking<F>(&self, done: Completion, work: F)
    where
        F: FnOnce() -> Result<PlatformValue, PlatformError> + Send + 'static,
    {
        self.runtime.spawn_blocking(move || done.finish(work()));
    }
}

/// GUI-thread-only state (native objects that are not `Send`).
pub(crate) struct PlatformState {
    pub audio: AudioEngine,
}

pub struct Platform {
    shared: Arc<Shared>,
    state: PlatformState,
}

#[derive(Clone)]
pub struct PlatformHandle(pub(crate) Arc<Shared>);

static HANDLE: OnceLock<PlatformHandle> = OnceLock::new();

/// The process-global handle, available after `Platform::start`.
pub fn platform() -> Option<PlatformHandle> {
    HANDLE.get().cloned()
}

/// Runs one request to completion on the calling (non-GUI) thread. Used by the
/// blocking facades (`desktop::invoke`, `notifications::*`, the tone test).
pub(crate) fn blocking(
    start: impl FnOnce(&Arc<Shared>, Completion),
) -> Result<PlatformValue, PlatformError> {
    let handle = platform()
        .ok_or_else(|| PlatformError::new("not_started", "The platform layer has not started."))?;
    let id = handle.0.next_id();
    let receiver = crate::action::wait_for(id);
    start(&handle.0, Completion(id));
    receiver
        .recv()
        .map_err(|_| PlatformError::native("The platform request was dropped."))?
}

impl Platform {
    pub fn start(cx: &mut Cx, config: PlatformConfig) -> anyhow::Result<Platform> {
        anyhow::ensure!(
            HANDLE.get().is_none(),
            "Platform::start was already called."
        );
        native::prepare_process(cx)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("paseo-platform")
            .enable_all()
            .build()?;
        let audio = AudioEngine::default();
        let shared = Arc::new(Shared {
            next_request: AtomicU64::new(1),
            runtime,
            main_tasks: Mutex::new(Vec::new()),
            audio: audio.queue(),
            insets: Mutex::new(SafeInsets::default()),
            #[cfg(any(target_os = "android", target_os = "ios"))]
            core: config.core.clone(),
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            desktop: crate::desktop::DesktopShared::new(&config)?,
            #[cfg(any(target_os = "android", target_os = "ios"))]
            mobile: crate::mobile::MobileShared::new(),
        });
        let state = PlatformState { audio };
        let _ = HANDLE.set(PlatformHandle(shared.clone()));
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        crate::desktop::start(&shared, &config);
        #[cfg(any(target_os = "android", target_os = "ios"))]
        let state = {
            let mut state = state;
            crate::mobile::start(cx, &shared, &mut state);
            state
        };
        Ok(Platform { shared, state })
    }

    pub fn handle(&self) -> PlatformHandle {
        PlatformHandle(self.shared.clone())
    }

    pub fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        let tasks = std::mem::take(&mut *self.shared.main_tasks.lock().unwrap());
        for task in tasks {
            task(cx, &mut self.state);
        }
        match event {
            Event::AudioDevices(devices) => self.state.audio.devices(cx, devices),
            Event::Signal => self.state.audio.update(cx),
            _ => {}
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        crate::desktop::handle_event(&self.shared);
        #[cfg(any(target_os = "android", target_os = "ios"))]
        crate::mobile::handle_event(cx, &self.shared, &mut self.state, event);
    }

    pub fn finish_event(&mut self, cx: &mut Cx, event: &Event) {
        #[cfg(any(target_os = "android", target_os = "ios"))]
        crate::mobile::finish_event(&self.shared, event);
        if let Event::BackPressed { handled } = event
            && !handled.get()
        {
            // Leaving the app must not stop remote sessions: background the task.
            native::background_app(cx);
            handled.set(true);
        }
    }

    pub fn observe_core_event(&mut self, event: &CoreEvent) {
        #[cfg(any(target_os = "android", target_os = "ios"))]
        crate::mobile::observe_core_event(&self.shared, event);
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        crate::desktop::observe_core_event(&self.shared, event);
    }

    pub fn window_ready(&mut self) {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        self.shared.desktop.window_ready();
        #[cfg(any(target_os = "android", target_os = "ios"))]
        self.shared.mobile.window_ready();
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
impl Drop for Platform {
    fn drop(&mut self) {
        crate::mobile::shutdown(&self.shared);
    }
}

fn validate_external_url(url: &str) -> Result<String, PlatformError> {
    let parsed = url::Url::parse(url.trim())
        .map_err(|_| PlatformError::invalid(format!("Not a valid URL: {url}")))?;
    match parsed.scheme() {
        "http" | "https" | "mailto" | "paseo" => Ok(parsed.to_string()),
        scheme => Err(PlatformError::invalid(format!(
            "Refusing to open a {scheme}: link."
        ))),
    }
}

impl PlatformHandle {
    fn shared(&self) -> &Arc<Shared> {
        &self.0
    }

    #[cfg(target_os = "android")]
    pub(crate) fn network_changed(&self) {
        self.0.core.network_changed();
    }

    pub fn capabilities(&self) -> Capabilities {
        native::capabilities()
    }

    pub fn open_external(&self, url: &str) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        match validate_external_url(url) {
            Ok(url) => native::open_external(self.shared(), url, done),
            Err(error) => done.fail(error),
        }
        id
    }

    pub fn open_path(&self, path: &Path) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        if !path.is_absolute() {
            done.fail(PlatformError::invalid(format!(
                "Paths must be absolute: {}",
                path.display()
            )));
        } else {
            native::open_path(self.shared(), path.to_owned(), done);
        }
        id
    }

    pub fn reveal_path(&self, path: &Path) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::reveal_path(self.shared(), path.to_owned(), done);
        id
    }

    pub fn pick_files(&self, options: PickFilesOptions) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::pick_files(self.shared(), options, done);
        id
    }

    pub fn pick_directory(&self, title: Option<String>) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::pick_directory(self.shared(), title, done);
        id
    }

    pub fn save_location(&self, suggested_name: &str) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::save_location(self.shared(), suggested_name.to_owned(), done);
        id
    }

    /// Copy a generated file to a user-selected native document destination.
    /// The caller retains the source until completion; cancellation is `Bool(false)`.
    pub fn save_file(&self, path: &Path, mime_type: &str, name: &str) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::save_file(
            self.shared(),
            path.to_owned(),
            mime_type.to_owned(),
            name.to_owned(),
            done,
        );
        id
    }

    /// Save an image into the device's photo library without a location prompt (Android:
    /// Pictures/Passession in the gallery; iOS: Photos). Only where
    /// [`Capabilities::photo_library`] is true; completes `Bool(true)` once saved and
    /// `Bool(false)` when a fallback save dialog is cancelled. The caller retains the source
    /// until completion.
    pub fn save_image(&self, path: &Path, mime_type: &str, name: &str) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::save_image(
            self.shared(),
            path.to_owned(),
            mime_type.to_owned(),
            name.to_owned(),
            done,
        );
        id
    }

    pub fn share_file(&self, path: &Path, mime_type: &str, name: &str) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::share_file(
            self.shared(),
            path.to_owned(),
            mime_type.to_owned(),
            name.to_owned(),
            done,
        );
        id
    }

    pub fn transfer_staging_file(&self, name: &str) -> std::io::Result<PathBuf> {
        crate::paths::transfer_staging_file(name)
    }

    pub fn copy_text(&self, text: String) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::copy_text(self.shared(), text, done);
        id
    }

    pub fn read_clipboard(&self, images_only: bool) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::read_clipboard(self.shared(), images_only, done);
        id
    }

    pub fn copy_image(&self, bytes: Vec<u8>, mime_type: &str) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::copy_image(self.shared(), bytes, mime_type.to_owned(), done);
        id
    }

    pub fn notify(&self, request: NotificationRequest) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::notify(self.shared(), request, done);
        id
    }

    /// Shows the OS notification prompt when it still can and completes with
    /// the resulting [`PermissionState`]. On mobile a grant also registers
    /// push on builds provisioned for it.
    pub fn request_notification_permission(&self) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::request_notification_permission(self.shared(), done);
        id
    }

    /// The current notification permission, without prompting.
    pub fn notification_permission_status(&self) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::notification_permission_status(self.shared(), done);
        id
    }

    /// Whether this mobile build has push credentials, independent of permission.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub fn push_availability(&self) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        let shared = self.0.clone();
        self.0.spawn(async move {
            done.finish(
                shared
                    .mobile
                    .push_available()
                    .await
                    .map(PlatformValue::Bool),
            );
        });
        id
    }

    /// Opens the system settings screen for `target`; completes with
    /// `Bool(true)` once it opened, or fails `unsupported` where the OS has no
    /// such screen. Recheck the permission when the app returns to the
    /// foreground.
    pub fn open_permission_settings(&self, target: PermissionSettings) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::open_permission_settings(self.shared(), target, done);
        id
    }

    /// Android power management that may stop the app in the background:
    /// `Json({"batteryOptimized": bool, "backgroundRestricted": bool})`.
    /// Fails `unsupported` on other platforms. Never prompts.
    pub fn background_restrictions(&self) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::background_restrictions(self.shared(), done);
        id
    }

    pub fn set_badge_count(&self, count: u32) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::set_badge_count(self.shared(), count, done);
        id
    }

    pub fn idle_milliseconds(&self) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::idle_milliseconds(self.shared(), done);
        id
    }

    /// Plays after everything already queued; completes with `Bool(true)` when
    /// finished or `Bool(false)` when cancelled.
    pub fn play_audio(&self, bytes: Vec<u8>, mime_type: &str) -> PlatformRequestId {
        let id = self.0.next_id();
        submit_audio(&self.0, id, bytes, mime_type.to_owned());
        id
    }

    pub fn cancel_audio(&self) {
        self.0.on_main(|cx, state| state.audio.cancel(cx));
    }

    /// `request` shows the OS prompt when it still can; `false` only reads the
    /// current state.
    pub fn camera_permission(&self, request: bool) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        native::camera_permission(self.shared(), request, done);
        id
    }

    pub fn safe_insets(&self) -> SafeInsets {
        *self.0.insets.lock().unwrap()
    }

    pub fn report_error(&self, message: &str) {
        native::report_error(self.shared(), message);
    }

    /// Shows (`Some`) or removes (`None`) the Android background presence of the
    /// embedded Tailscale connection; its controls arrive as
    /// [`PlatformEvent::TailnetControl`](crate::PlatformEvent::TailnetControl).
    #[cfg(target_os = "android")]
    pub fn set_tailnet_presence(&self, presence: Option<crate::action::TailnetPresence>) {
        native::set_tailnet_presence(self.shared(), presence);
    }

    // ----- Desktop host -----

    pub fn desktop_invoke(&self, command: &str, args: Value) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        crate::desktop::invoke_command(self.shared(), command.to_owned(), args, done);
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            let _ = (command, args);
            done.fail(PlatformError::unsupported(
                "Mobile clients never manage a local daemon.",
            ));
        }
        id
    }

    pub fn list_editor_targets(&self) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        self.0.complete_blocking(done, || {
            Ok(PlatformValue::EditorTargets(
                crate::desktop::editors::list_editor_targets(),
            ))
        });
        #[cfg(any(target_os = "android", target_os = "ios"))]
        done.ok(PlatformValue::EditorTargets(Vec::new()));
        id
    }

    pub fn open_editor(&self, request: OpenEditorRequest) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        self.0.complete_blocking(done, move || {
            crate::desktop::editors::open_editor(&request).map(|()| PlatformValue::None)
        });
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            let _ = request;
            done.fail(PlatformError::unsupported(
                "Opening a local editor is a desktop feature.",
            ));
        }
        id
    }

    /// Returns true if closing this window requests a guarded application quit.
    pub fn window_close_requested(&self, open_windows: usize) -> bool {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            crate::desktop::window_close_requested(self.shared(), open_windows)
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            let _ = open_windows;
            false
        }
    }

    /// Hides one window without destroying it (the dormant first window kept
    /// for the next New Window). GUI thread. The op runs on Makepad's own
    /// top-level frame for `window` when the platform ops are processed
    /// (Windows: the `CreateWindowExW` frame HWND, resolved then, never a
    /// cached child handle). Routes from notification clicks and links can reopen it.
    /// Android and iOS have one OS-owned window; there this does nothing.
    pub fn hide_window(&self, cx: &mut Cx, window: makepad_widgets::WindowId) {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            cx.push_unique_platform_op(makepad_widgets::CxOsOp::HideWindow(window));
            self.0.desktop.set_dormant(true);
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        let _ = (cx, window);
    }

    /// Shows a window hidden with [`Self::hide_window`] (restoring it if
    /// minimized) and activates it. GUI thread; same frame resolution.
    pub fn show_window(&self, cx: &mut Cx, window: makepad_widgets::WindowId) {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            cx.push_unique_platform_op(makepad_widgets::CxOsOp::Deminiaturize(window));
            self.0.desktop.set_dormant(false);
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        let _ = (cx, window);
    }

    pub fn window_geometry(&self, key: &str) -> Option<WindowGeometry> {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        return self.0.desktop.geometry.get(key);
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            let _ = key;
            None
        }
    }

    pub fn record_window_geometry(&self, key: &str, geometry: WindowGeometry) {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        self.0.desktop.geometry.record(&self.0, key, geometry);
        #[cfg(any(target_os = "android", target_os = "ios"))]
        let _ = (key, geometry);
    }

    pub fn request_quit(&self, reason: QuitReason, force: bool) {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        crate::desktop::request_quit(self.shared(), reason, force);
        #[cfg(any(target_os = "android", target_os = "ios"))]
        let _ = (reason, force);
    }

    pub fn respond_quit(&self, token: u64, allowed: bool) {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        crate::desktop::respond_quit(self.shared(), token, allowed);
        #[cfg(any(target_os = "android", target_os = "ios"))]
        let _ = (token, allowed);
    }

    // ----- Mobile -----

    pub fn revoke_push_host(&self, host_id: &str) -> PlatformRequestId {
        let done = self.0.completion();
        let id = done.id();
        #[cfg(any(target_os = "android", target_os = "ios"))]
        crate::mobile::revoke_host(self.shared(), host_id.to_owned(), done);
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            let _ = host_id;
            done.ok(PlatformValue::None);
        }
        id
    }
}

/// Local file paths from a Makepad drop event (`DragItem::FilePath`).
pub fn dropped_file_paths(event: &makepad_widgets::DropEvent) -> Vec<PathBuf> {
    event
        .items
        .iter()
        .filter_map(|item| match item {
            makepad_widgets::DragItem::FilePath { path, .. } => Some(PathBuf::from(path)),
            _ => None,
        })
        .collect()
}

/// Admits `id` to the FIFO (fixing its place) and decodes on a worker.
pub(crate) fn submit_audio(
    shared: &Arc<Shared>,
    id: PlatformRequestId,
    bytes: Vec<u8>,
    mime: String,
) {
    let queue = shared.audio.clone();
    let generation = queue.admit(id);
    shared.spawn_blocking(move || {
        let decoded = audio::decode_source(&bytes, &mime);
        queue.decoded(id, generation, decoded);
    });
    SignalToUI::set_ui_signal();
}
