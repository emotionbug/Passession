//! WebView2's child window sits inside a Win32 child sized to the visible part of the card.
//! Makepad's Win32 loop dispatches WebView2's STA callbacks; none wait or pump messages.

use super::{WebViewEvent, WebViewOptions, bootstrap, parse_message, wrap_html};
use crate::desktop::windows_util::Wide;
use makepad_widgets::{
    Cx, Rect, SignalToUI, WindowId, makepad_platform::os::windows::win32_app::window_hwnd,
};
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::LazyLock,
};
use webview2_com::{
    AddScriptToExecuteOnDocumentCreatedCompletedHandler, CoTaskMemPWSTR,
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    DownloadStartingEventHandler, LaunchingExternalUriSchemeEventHandler,
    Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_BOUNDS_MODE_USE_RAW_PIXELS, COREWEBVIEW2_COLOR,
        COREWEBVIEW2_PERMISSION_STATE_DENY, COREWEBVIEW2_PROCESS_FAILED_KIND,
        COREWEBVIEW2_WEB_ERROR_STATUS, CreateCoreWebView2EnvironmentWithOptions,
        GetAvailableCoreWebView2BrowserVersionString, ICoreWebView2, ICoreWebView2_4,
        ICoreWebView2_18, ICoreWebView2Controller, ICoreWebView2Controller2,
        ICoreWebView2Controller3, ICoreWebView2Controller4, ICoreWebView2Environment,
        ICoreWebView2Settings3, ICoreWebView2Settings4, ICoreWebView2Settings5,
        ICoreWebView2Settings6,
    },
    NavigationCompletedEventHandler, NavigationStartingEventHandler,
    NewWindowRequestedEventHandler, PermissionRequestedEventHandler, ProcessFailedEventHandler,
    WebMessageReceivedEventHandler, take_pwstr,
};
use windows::{
    Win32::{
        Foundation::{HWND, POINT, RECT},
        Graphics::Gdi::ClientToScreen,
        UI::{
            Input::KeyboardAndMouse::{GetFocus, SetFocus},
            WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, IsChild, IsWindow, SW_HIDE, SW_SHOWNA,
                SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos, ShowWindow, WS_CHILD, WS_CLIPCHILDREN,
                WS_CLIPSIBLINGS, WS_EX_NOPARENTNOTIFY,
            },
        },
    },
    core::{BOOL, Interface, PCWSTR, PWSTR, w},
};

static RUNTIME: LazyLock<Result<(), String>> = LazyLock::new(|| {
    let mut version = PWSTR::null();
    let result =
        unsafe { GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut version) };
    let _version = CoTaskMemPWSTR::from(version);
    result.map_err(|error| format!("WebView2 Runtime is unavailable: {error}"))
});

pub(super) fn supported() -> bool {
    static REPORTED: std::sync::Once = std::sync::Once::new();
    if let Err(error) = &*RUNTIME {
        REPORTED.call_once(|| makepad_widgets::log!("Web views unavailable: {error}"));
        return false;
    }
    true
}

#[derive(Clone, Copy, PartialEq)]
struct Placement {
    clip: RECT,
    bounds: RECT,
    dpi: f64,
}

impl Placement {
    fn new(frame: Rect, clip: Rect, dpi: f64) -> Self {
        let px = |value: f64| (value * dpi).round() as i32;
        let edges = |rect: Rect| RECT {
            left: px(rect.pos.x),
            top: px(rect.pos.y),
            right: px(rect.pos.x + rect.size.x),
            bottom: px(rect.pos.y + rect.size.y),
        };
        let clip = edges(clip);
        let frame = edges(frame);
        Self {
            clip,
            bounds: RECT {
                left: frame.left - clip.left,
                top: frame.top - clip.top,
                right: frame.right - clip.left,
                bottom: frame.bottom - clip.top,
            },
            dpi,
        }
    }
}

struct Controller(ICoreWebView2Controller);

impl Drop for Controller {
    fn drop(&mut self) {
        // Close unregisters all the controller's and page's event handlers.
        let _ = unsafe { self.0.Close() };
    }
}

struct Page {
    controller: Controller,
    scaling: ICoreWebView2Controller3,
    core: ICoreWebView2,
}

enum Setup {
    Environment(ICoreWebView2Environment),
    Controller(Controller),
    Navigate(Wide),
}

#[derive(Clone, Copy, Default)]
enum Navigation {
    #[default]
    Locked,
    AllowInitial,
    Initial(u64),
}

#[derive(Default)]
struct Shared {
    setup: Cell<Option<Result<Setup, String>>>,
    navigation: Cell<Navigation>,
    events: RefCell<Vec<WebViewEvent>>,
}

impl Shared {
    fn complete(shared: &Weak<Self>, stage: &str, result: Result<Setup, windows::core::Error>) {
        if let Some(shared) = shared.upgrade() {
            shared.setup.set(Some(
                result.map_err(|error| format!("WebView2 {stage}: {error}")),
            ));
            SignalToUI::set_ui_signal();
        }
    }

    fn post(&self, event: WebViewEvent) {
        self.events.borrow_mut().push(event);
        SignalToUI::set_ui_signal();
    }
}

pub(super) struct WebView {
    window: WindowId,
    parent: HWND,
    clip: HWND,
    options: Option<WebViewOptions>,
    shared: Rc<Shared>,
    page: Option<Page>,
    placement: Option<Placement>,
    applied: Option<Placement>,
    screen_origin: Option<POINT>,
    shown: bool,
    ready: bool,
    scripts: Vec<String>,
}

impl WebView {
    pub(super) fn new(
        _: &mut Cx,
        window: WindowId,
        options: WebViewOptions,
    ) -> Result<Self, String> {
        RUNTIME.as_ref().map_err(Clone::clone)?;
        let parent = window_hwnd(window).ok_or("No Makepad window for WebView2")?;
        let state_override =
            std::env::var_os("PASEO_GUI_STATE_DIR").is_some_and(|dir| !dir.is_empty());
        let app_data = if state_override {
            crate::paths::app_paths().support_dir.clone()
        } else {
            dirs::data_local_dir()
                .ok_or("No local app data directory for WebView2")?
                .join("Paseo")
                .join("Paseo Sessions")
        };
        let user_data = Wide::new(app_data.join("WebView2"));
        let clip = unsafe {
            CreateWindowExW(
                WS_EX_NOPARENTNOTIFY,
                w!("STATIC"),
                PCWSTR::null(),
                WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
                0,
                0,
                1,
                1,
                Some(parent),
                None,
                None,
                None,
            )
        }
        .map_err(|error| format!("Creating WebView2's clip window: {error}"))?;
        let view = Self {
            window,
            parent,
            clip,
            options: Some(options),
            shared: Rc::default(),
            page: None,
            placement: None,
            applied: None,
            screen_origin: None,
            shown: false,
            ready: false,
            scripts: Vec::new(),
        };
        let shared = Rc::downgrade(&view.shared);
        let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
            move |status, environment| {
                let result = status.and_then(|()| environment.ok_or_else(missing_interface));
                Shared::complete(
                    &shared,
                    "environment creation",
                    result.map(Setup::Environment),
                );
                Ok(())
            },
        ));
        // Makepad has already called OleInitialize on this thread.
        unsafe {
            CreateCoreWebView2EnvironmentWithOptions(
                PCWSTR::null(),
                user_data.pcwstr(),
                None,
                &handler,
            )
        }
        .map_err(|error| format!("Starting WebView2: {error}"))?;
        Ok(view)
    }

    fn advance(&mut self, cx: &mut Cx, setup: Setup) -> Result<(), windows::core::Error> {
        if window_hwnd(self.window) != Some(self.parent) {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "The Makepad window was closed",
            ));
        }
        match setup {
            Setup::Environment(environment) => {
                let shared = Rc::downgrade(&self.shared);
                let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
                    move |status, controller| {
                        let controller = controller.map(Controller);
                        let result = status.and_then(|()| controller.ok_or_else(missing_interface));
                        Shared::complete(
                            &shared,
                            "controller creation",
                            result.map(Setup::Controller),
                        );
                        Ok(())
                    },
                ));
                unsafe { environment.CreateCoreWebView2Controller(self.clip, &handler) }
            }
            Setup::Controller(controller) => {
                let options = self.options.take().expect("one WebView2 controller");
                let page = Page::configure(controller, &options, &self.shared)?;
                let html = Wide::new(wrap_html(&options.html, &options.theme));
                let bootstrap = bootstrap(
                    &options,
                    "(message) => window.chrome.webview.postMessage(message)",
                );
                // The document-created script runs before parsing; the bridge needs the form.
                // Block native link and form actions, including same-document navigations.
                let script = Wide::new(format!(
                    "if (window === window.top) {{
                        for (const type of ['click', 'auxclick'])
                            document.addEventListener(type, event => {{
                                if (event.composedPath().some(node => node.localName === 'a' || node.localName === 'area'))
                                    event.preventDefault();
                            }}, true);
                        document.addEventListener('submit', event => event.preventDefault(), true);
                        document.addEventListener('DOMContentLoaded', () => {{ {bootstrap} }}, {{ once: true }});
                    }}"
                ));
                let shared = Rc::downgrade(&self.shared);
                let handler = AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(
                    Box::new(move |status, _| {
                        Shared::complete(
                            &shared,
                            "script registration",
                            status.map(|()| Setup::Navigate(html)),
                        );
                        Ok(())
                    }),
                );
                unsafe {
                    page.core
                        .AddScriptToExecuteOnDocumentCreated(script.pcwstr(), &handler)?
                };
                self.page = Some(page);
                self.apply_placement(cx)
            }
            Setup::Navigate(html) => {
                let page = self.page.as_ref().expect("WebView2 script registered");
                self.shared.navigation.set(Navigation::AllowInitial);
                unsafe { page.core.NavigateToString(html.pcwstr()) }
            }
        }
    }

    pub(super) fn place(&mut self, cx: &mut Cx, frame: Rect, visible: Option<Rect>, dpi: f64) {
        self.placement = visible.map(|clip| Placement::new(frame, clip, dpi));
        if let Err(error) = self.apply_placement(cx) {
            self.shared
                .post(WebViewEvent::Failed(format!("Placing WebView2: {error}")));
        }
    }

    fn apply_placement(&mut self, cx: &mut Cx) -> windows::core::Result<()> {
        let show = self.placement.is_some() && self.ready;
        if self.shown == show && self.applied == self.placement {
            return Ok(());
        }
        let repaint = self.shown;
        if repaint && !show {
            self.blur();
            let _ = unsafe { ShowWindow(self.clip, SW_HIDE) };
            self.shown = false;
            self.screen_origin = None;
        }
        let Some(page) = &self.page else {
            return Ok(());
        };
        unsafe {
            if repaint && !show {
                page.controller.0.SetIsVisible(false)?;
            }
            if let Some(placement) = self.placement {
                if self.applied.is_none_or(|old| old.dpi != placement.dpi) {
                    page.scaling.SetRasterizationScale(placement.dpi)?;
                }
                if self
                    .applied
                    .is_none_or(|old| old.bounds != placement.bounds)
                {
                    page.controller.0.SetBounds(placement.bounds)?;
                }
                if self.applied.is_none_or(|old| old.clip != placement.clip) {
                    let rect = placement.clip;
                    SetWindowPos(
                        self.clip,
                        None,
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOACTIVATE | SWP_NOZORDER,
                    )?;
                }
                if show && !self.shown {
                    page.controller.0.SetIsVisible(true)?;
                    let _ = ShowWindow(self.clip, SW_SHOWNA);
                    self.shown = true;
                }
            }
        }
        self.applied = self.placement;
        // The clip may have covered pixels Makepad didn't need to draw before.
        if repaint && let Some(pass) = cx.windows[self.window].main_pass_id {
            cx.repaint_pass(pass);
        }
        Ok(())
    }

    pub(super) fn run_script(&mut self, script: &str) {
        if !self.ready {
            self.scripts.push(script.to_owned());
            return;
        }
        if let Some(page) = &self.page
            && let Err(error) = unsafe { page.core.ExecuteScript(Wide::new(script).pcwstr(), None) }
        {
            self.shared.post(WebViewEvent::Failed(format!(
                "Running WebView2 script: {error}"
            )));
        }
    }

    pub(super) fn blur(&mut self) {
        unsafe {
            let focus = GetFocus();
            if focus == self.clip || IsChild(self.clip, focus).as_bool() {
                let _ = SetFocus(Some(self.parent));
            }
        }
    }

    pub(super) fn pump(&mut self, cx: &mut Cx) -> Vec<WebViewEvent> {
        if let Some(setup) = self.shared.setup.take()
            && let Err(error) = setup.and_then(|setup| {
                self.advance(cx, setup)
                    .map_err(|error| format!("Preparing WebView2: {error}"))
            })
        {
            self.shared.post(WebViewEvent::Failed(error));
        }
        let mut events = std::mem::take(&mut *self.shared.events.borrow_mut());
        if events
            .iter()
            .any(|event| matches!(event, WebViewEvent::Failed(_)))
        {
            return events;
        }
        // The bridge's first height report means the parsed form is ready for queued scripts.
        if !self.ready
            && events
                .iter()
                .any(|event| matches!(event, WebViewEvent::Height(_)))
        {
            self.ready = true;
            if let Err(error) = self.apply_placement(cx) {
                events.push(WebViewEvent::Failed(format!("Showing WebView2: {error}")));
                return events;
            }
            for script in std::mem::take(&mut self.scripts) {
                self.run_script(&script);
            }
        }
        if self.shown
            && let Some(page) = &self.page
        {
            // IME and select popups follow the screen position even when only the host moved.
            let mut origin = POINT::default();
            unsafe {
                if ClientToScreen(self.clip, &mut origin).as_bool()
                    && self.screen_origin != Some(origin)
                {
                    self.screen_origin = Some(origin);
                    let _ = page.controller.0.NotifyParentWindowPositionChanged();
                }
            }
        }
        events
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        self.blur();
        drop(self.shared.setup.take());
        drop(self.page.take());
        unsafe {
            if IsWindow(Some(self.clip)).as_bool() {
                let _ = DestroyWindow(self.clip);
            }
        }
    }
}

impl Page {
    fn configure(
        controller: Controller,
        options: &WebViewOptions,
        shared: &Rc<Shared>,
    ) -> windows::core::Result<Self> {
        let core = unsafe { controller.0.CoreWebView2()? };
        let scaling = controller.0.cast::<ICoreWebView2Controller3>()?;
        let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        let bg = options.theme.background;
        unsafe {
            controller.0.SetIsVisible(false)?;
            controller
                .0
                .cast::<ICoreWebView2Controller2>()?
                .SetDefaultBackgroundColor(COREWEBVIEW2_COLOR {
                    A: if bg.w > 0.0 { 255 } else { 0 },
                    R: channel(bg.x),
                    G: channel(bg.y),
                    B: channel(bg.z),
                })?;
            // Makepad supplies device bounds and the logical-to-device scale, including any
            // app DPI override. WebView2 applies that scale to CSS px, not to the raw bounds.
            scaling.SetBoundsMode(COREWEBVIEW2_BOUNDS_MODE_USE_RAW_PIXELS)?;
            scaling.SetShouldDetectMonitorScaleChanges(false)?;
            controller.0.SetZoomFactor(1.0)?;
            controller
                .0
                .cast::<ICoreWebView2Controller4>()?
                .SetAllowExternalDrop(false)?;
            let settings = core.Settings()?;
            // wrap_html's CSP blocks the document's scripts and inline handlers. Scripting
            // itself stays on: `SetIsScriptEnabled(false)` also stops scripts added with
            // AddScriptToExecuteOnDocumentCreated, the bridge (WebView2Feedback#466).
            settings.SetAreHostObjectsAllowed(false)?;
            settings.SetAreDefaultContextMenusEnabled(false)?;
            settings.SetAreDevToolsEnabled(false)?;
            settings.SetIsStatusBarEnabled(false)?;
            settings.SetIsZoomControlEnabled(false)?;
            settings.SetIsBuiltInErrorPageEnabled(false)?;
            settings
                .cast::<ICoreWebView2Settings3>()?
                .SetAreBrowserAcceleratorKeysEnabled(false)?;
            let autofill = settings.cast::<ICoreWebView2Settings4>()?;
            autofill.SetIsPasswordAutosaveEnabled(false)?;
            autofill.SetIsGeneralAutofillEnabled(false)?;
            settings
                .cast::<ICoreWebView2Settings5>()?
                .SetIsPinchZoomEnabled(false)?;
            settings
                .cast::<ICoreWebView2Settings6>()?
                .SetIsSwipeNavigationEnabled(false)?;
        }
        Self::subscribe(&core, shared)?;
        Ok(Self {
            controller,
            scaling,
            core,
        })
    }

    fn subscribe(core: &ICoreWebView2, shared: &Rc<Shared>) -> windows::core::Result<()> {
        let mut token = 0;
        let starting = Rc::downgrade(shared);
        let completed = Rc::downgrade(shared);
        let failed = Rc::downgrade(shared);
        let message = Rc::downgrade(shared);
        unsafe {
            core.add_NavigationStarting(
                &NavigationStartingEventHandler::create(Box::new(move |_, args| {
                    let args = args.ok_or_else(missing_interface)?;
                    if let Some(shared) = starting.upgrade()
                        && matches!(shared.navigation.get(), Navigation::AllowInitial)
                    {
                        // Only the host's first NavigateToString is authorized, not redirects.
                        let mut id = 0;
                        args.NavigationId(&mut id)?;
                        shared.navigation.set(Navigation::Initial(id));
                        return Ok(());
                    }
                    args.SetCancel(true)
                })),
                &mut token,
            )?;
            core.add_FrameNavigationStarting(
                &NavigationStartingEventHandler::create(Box::new(|_, args| {
                    args.ok_or_else(missing_interface)?.SetCancel(true)
                })),
                &mut token,
            )?;
            core.add_NavigationCompleted(
                &NavigationCompletedEventHandler::create(Box::new(move |_, args| {
                    let (Some(shared), Some(args)) = (completed.upgrade(), args) else {
                        return Ok(());
                    };
                    let mut id = 0;
                    args.NavigationId(&mut id)?;
                    if matches!(shared.navigation.get(), Navigation::Initial(initial) if initial == id) {
                        shared.navigation.set(Navigation::Locked);
                        let mut success = BOOL::default();
                        args.IsSuccess(&mut success)?;
                        if !success.as_bool() {
                            let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
                            args.WebErrorStatus(&mut status)?;
                            shared.post(WebViewEvent::Failed(format!(
                                "Loading WebView2 form: {status:?}"
                            )));
                        }
                    }
                    Ok(())
                })),
                &mut token,
            )?;
            core.add_NewWindowRequested(
                &NewWindowRequestedEventHandler::create(Box::new(|_, args| {
                    args.ok_or_else(missing_interface)?.SetHandled(true)
                })),
                &mut token,
            )?;
            core.cast::<ICoreWebView2_4>()?.add_DownloadStarting(
                &DownloadStartingEventHandler::create(Box::new(|_, args| {
                    args.ok_or_else(missing_interface)?.SetCancel(true)
                })),
                &mut token,
            )?;
            core.cast::<ICoreWebView2_18>()?
                .add_LaunchingExternalUriScheme(
                    &LaunchingExternalUriSchemeEventHandler::create(Box::new(|_, args| {
                        args.ok_or_else(missing_interface)?.SetCancel(true)
                    })),
                    &mut token,
                )?;
            core.add_PermissionRequested(
                &PermissionRequestedEventHandler::create(Box::new(|_, args| {
                    args.ok_or_else(missing_interface)?
                        .SetState(COREWEBVIEW2_PERMISSION_STATE_DENY)
                })),
                &mut token,
            )?;
            core.add_ProcessFailed(
                &ProcessFailedEventHandler::create(Box::new(move |_, args| {
                    if let Some(shared) = failed.upgrade() {
                        let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
                        if let Some(args) = args {
                            args.ProcessFailedKind(&mut kind)?;
                        }
                        shared.post(WebViewEvent::Failed(format!(
                            "WebView2 process failed: {kind:?}"
                        )));
                    }
                    Ok(())
                })),
                &mut token,
            )?;
            core.add_WebMessageReceived(
                &WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
                    if let (Some(shared), Some(args)) = (message.upgrade(), args) {
                        let mut text = PWSTR::null();
                        if args.TryGetWebMessageAsString(&mut text).is_ok()
                            && let Some(event) = parse_message(&take_pwstr(text))
                        {
                            shared.post(event);
                        }
                    }
                    Ok(())
                })),
                &mut token,
            )?;
        }
        Ok(())
    }
}

fn missing_interface() -> windows::core::Error {
    windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER)
}
