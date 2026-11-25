//! A WKWebView in a clipped native subview of Makepad's rendering view.

use super::{WebViewEvent, WebViewOptions, bootstrap, parse_message, wrap_html};
use block2::DynBlock;
use makepad_widgets::{Cx, Rect, Vec4, WindowId};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_foundation::{NSError, NSPoint, NSRect, NSSize, NSString, ns_string};
use objc2_web_kit::{
    WKContentWorld, WKNavigation, WKNavigationAction, WKNavigationActionPolicy,
    WKNavigationDelegate, WKScriptMessage, WKScriptMessageHandler, WKUIDelegate,
    WKUserContentController, WKUserScript, WKUserScriptInjectionTime, WKWebViewConfiguration,
    WKWebpagePreferences, WKWebsiteDataStore,
};
use std::cell::{Cell, RefCell};
use std::sync::LazyLock;

#[cfg(target_os = "macos")]
use makepad_widgets::makepad_platform::os::apple::macos::macos_app::webview_host;
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSColor, NSView as NativeView};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSNumber, NSObjectNSKeyValueCoding};
#[cfg(target_os = "macos")]
use objc2_web_kit::WKWebView;

#[cfg(target_os = "ios")]
use makepad_widgets::makepad_platform::os::apple::ios::ios_app::webview_host;
#[cfg(target_os = "ios")]
use objc2::rc::Allocated;
#[cfg(target_os = "ios")]
use objc2::runtime::AnyObject;
#[cfg(target_os = "ios")]
use objc2::{extern_class, extern_methods};
#[cfg(target_os = "ios")]
use objc2_foundation::NSURL;
#[cfg(target_os = "ios")]
use objc2_ui_kit::{
    UIColor, UIContextMenuConfiguration, UIGestureRecognizer, UIGestureRecognizerDelegate,
    UIGestureRecognizerState, UIPanGestureRecognizer, UIResponder, UIScrollView,
    UIScrollViewContentInsetAdjustmentBehavior, UIView as NativeView,
};
#[cfg(target_os = "ios")]
use objc2_web_kit::{WKContentMode, WKFrameInfo};

// objc2-web-kit 0.3 only binds the AppKit WKWebView. On iOS these same public
// WebKit selectors are implemented by a UIView subclass.
#[cfg(target_os = "ios")]
extern_class!(
    #[unsafe(super(NativeView, UIResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    struct WKWebView;
);

#[cfg(target_os = "ios")]
#[allow(non_snake_case)]
impl WKWebView {
    extern_methods!(
        #[unsafe(method(initWithFrame:configuration:))]
        #[unsafe(method_family = init)]
        unsafe fn initWithFrame_configuration(
            this: Allocated<Self>,
            frame: NSRect,
            configuration: &WKWebViewConfiguration,
        ) -> Retained<Self>;

        #[unsafe(method(setNavigationDelegate:))]
        unsafe fn setNavigationDelegate(
            &self,
            delegate: Option<&ProtocolObject<dyn WKNavigationDelegate>>,
        );

        #[unsafe(method(setUIDelegate:))]
        unsafe fn setUIDelegate(&self, delegate: Option<&ProtocolObject<dyn WKUIDelegate>>);

        #[unsafe(method(loadHTMLString:baseURL:))]
        unsafe fn loadHTMLString_baseURL(
            &self,
            html: &NSString,
            url: Option<&NSURL>,
        ) -> Option<Retained<WKNavigation>>;

        #[unsafe(method(evaluateJavaScript:inFrame:inContentWorld:completionHandler:))]
        unsafe fn evaluateJavaScript_inFrame_inContentWorld_completionHandler(
            &self,
            script: &NSString,
            frame: Option<&WKFrameInfo>,
            world: &WKContentWorld,
            completion: Option<&DynBlock<dyn Fn(*mut AnyObject, *mut NSError)>>,
        );

        #[unsafe(method(stopLoading))]
        unsafe fn stopLoading(&self);

        #[unsafe(method(setInspectable:))]
        unsafe fn setInspectable(&self, inspectable: bool);

        #[unsafe(method(setAllowsLinkPreview:))]
        unsafe fn setAllowsLinkPreview(&self, allows_link_preview: bool);

        #[unsafe(method(scrollView))]
        unsafe fn scrollView(&self) -> Retained<UIScrollView>;
    );
}

pub(super) fn supported() -> bool {
    static AVAILABLE: LazyLock<bool> = LazyLock::new(|| {
        let available =
            AnyClass::get(c"WKWebView").is_some() && AnyClass::get(c"WKContentWorld").is_some();
        if !available {
            makepad_widgets::log!("Web views unavailable: WebKit lacks isolated content worlds");
        }
        available
    });
    *AVAILABLE
}

struct State {
    allow_initial_load: Cell<bool>,
    initial_load: RefCell<Option<Retained<WKNavigation>>>,
    events: RefCell<Vec<WebViewEvent>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PaseoAskFormDelegate"]
    #[ivars = State]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl WKScriptMessageHandler for Delegate {
        #[allow(non_snake_case)]
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        unsafe fn userContentController_didReceiveScriptMessage(
            &self,
            _: &WKUserContentController,
            message: &WKScriptMessage,
        ) {
            if !unsafe { message.frameInfo().isMainFrame() } {
                return;
            }
            if let Ok(body) = unsafe { message.body() }.downcast::<NSString>() {
                if let Some(event) = parse_message(&body.to_string()) {
                    self.ivars().events.borrow_mut().push(event);
                }
            }
        }
    }

    unsafe impl WKNavigationDelegate for Delegate {}

    // The default policy when createWebViewWithConfiguration is absent is to cancel.
    unsafe impl WKUIDelegate for Delegate {}

    impl Delegate {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_navigation(
            &self,
            _: &WKWebView,
            action: &WKNavigationAction,
            decision: &DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            // loadHTMLString with no base URL uses about:blank. Only its first
            // main-frame action may pass; popup targets have no target frame.
            let initial = self.ivars().allow_initial_load.replace(false)
                && unsafe {
                    action.targetFrame().is_some_and(|frame| frame.isMainFrame())
                        && action
                            .request()
                            .URL()
                            .and_then(|url| url.absoluteString())
                            .is_some_and(|url| url.isEqualToString(ns_string!("about:blank")))
                };
            let policy = if initial {
                WKNavigationActionPolicy::Allow
            } else {
                WKNavigationActionPolicy::Cancel
            };
            decision.call((policy,));
        }

        #[unsafe(method(webView:didCommitNavigation:))]
        fn did_commit(&self, _: &WKWebView, _: Option<&WKNavigation>) {
            self.ivars().allow_initial_load.set(false);
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, _: &WKWebView, _: Option<&WKNavigation>) {
            self.ivars().initial_load.borrow_mut().take();
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(
            &self,
            _: &WKWebView,
            navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.navigation_failed(navigation, error);
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(&self, _: &WKWebView, navigation: Option<&WKNavigation>, error: &NSError) {
            self.navigation_failed(navigation, error);
        }

        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn process_terminated(&self, _: &WKWebView) {
            self.fail("WebKit's content process terminated".into());
        }

        #[cfg(target_os = "ios")]
        #[unsafe(method(webView:contextMenuConfigurationForElement:completionHandler:))]
        fn context_menu(
            &self,
            _: &WKWebView,
            _: &AnyObject,
            completion: &DynBlock<dyn Fn(*mut UIContextMenuConfiguration)>,
        ) {
            completion.call((std::ptr::null_mut(),));
        }

        #[cfg(target_os = "ios")]
        #[unsafe(method(paseoPan:))]
        fn pan(&self, pan: &UIPanGestureRecognizer) {
            let state: UIGestureRecognizerState = unsafe { msg_send![pan, state] };
            if matches!(
                state,
                UIGestureRecognizerState::Began
                    | UIGestureRecognizerState::Changed
                    | UIGestureRecognizerState::Ended
            ) {
                // UIWindow coordinates stay fixed while the clipped view follows the timeline.
                let dy = -pan.translationInView(None).y;
                pan.setTranslation_inView(NSPoint::ZERO, None);
                if dy != 0.0 {
                    self.ivars()
                        .events
                        .borrow_mut()
                        .push(WebViewEvent::Scroll { dx: 0.0, dy });
                }
            }
        }
    }

    #[cfg(target_os = "ios")]
    unsafe impl UIGestureRecognizerDelegate for Delegate {
        #[allow(non_snake_case)]
        #[unsafe(method(gestureRecognizerShouldBegin:))]
        fn gestureRecognizerShouldBegin(&self, recognizer: &UIGestureRecognizer) -> bool {
            recognizer
                .downcast_ref::<UIPanGestureRecognizer>()
                .is_some_and(|pan| {
                    let velocity = pan.velocityInView(None);
                    velocity.y.abs() > velocity.x.abs()
                })
        }

        #[allow(non_snake_case)]
        #[unsafe(method(gestureRecognizer:shouldRecognizeSimultaneouslyWithGestureRecognizer:))]
        fn gestureRecognizer_shouldRecognizeSimultaneouslyWithGestureRecognizer(
            &self,
            _: &UIGestureRecognizer,
            _: &UIGestureRecognizer,
        ) -> bool {
            true
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(State {
            allow_initial_load: Cell::new(true),
            initial_load: RefCell::default(),
            events: RefCell::default(),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn navigation_failed(&self, navigation: Option<&WKNavigation>, error: &NSError) {
        let mut initial = self.ivars().initial_load.borrow_mut();
        if initial
            .as_deref()
            .zip(navigation)
            .is_some_and(|(initial, failing)| std::ptr::eq(initial, failing))
        {
            initial.take();
            self.fail(error.localizedDescription().to_string());
        }
    }

    fn fail(&self, error: String) {
        self.ivars().allow_initial_load.set(false);
        self.ivars()
            .events
            .borrow_mut()
            .push(WebViewEvent::Failed(error));
    }
}

pub(super) struct WebView {
    host: Retained<NativeView>,
    container: Retained<NativeView>,
    view: Retained<WKWebView>,
    controller: Retained<WKUserContentController>,
    world: Retained<WKContentWorld>,
    delegate: Retained<Delegate>,
}

impl WebView {
    pub(super) fn new(
        _: &mut Cx,
        window: WindowId,
        options: WebViewOptions,
    ) -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("WebKit needs the main thread")?;
        if !supported() {
            return Err("WebKit does not support isolated content worlds".into());
        }
        // The native accessor returns a borrowed view from the same UI thread.
        let host = unsafe { Retained::retain(webview_host(window).cast::<NativeView>()) }
            .ok_or("Makepad's native view is not available")?;

        let delegate = Delegate::new(mtm);
        let (world, controller, view) = unsafe {
            let configuration = WKWebViewConfiguration::new(mtm);
            let preferences = WKWebpagePreferences::new(mtm);
            // Only page-authored JavaScript is blocked. WKUserScripts and app
            // evaluation still run; the client world shares the DOM, not globals.
            preferences.setAllowsContentJavaScript(false);
            #[cfg(target_os = "ios")]
            preferences.setPreferredContentMode(WKContentMode::Mobile);
            configuration.setDefaultWebpagePreferences(Some(&preferences));
            configuration.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore(mtm));

            let world = WKContentWorld::defaultClientWorld(mtm);
            let controller = WKUserContentController::new(mtm);
            let mut script = bootstrap(
                &options,
                "(message) => window.webkit.messageHandlers.paseo.postMessage(message)",
            );
            script.push_str(
                "\ndocument.addEventListener('contextmenu', event => event.preventDefault(), true);",
            );
            let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly_inContentWorld(
                WKUserScript::alloc(mtm),
                &NSString::from_str(&script),
                WKUserScriptInjectionTime::AtDocumentEnd,
                true,
                &world,
            );
            controller.addUserScript(&script);
            controller.addScriptMessageHandler_contentWorld_name(
                ProtocolObject::from_ref(&*delegate),
                &world,
                ns_string!("paseo"),
            );
            configuration.setUserContentController(&controller);

            let view = WKWebView::initWithFrame_configuration(
                WKWebView::alloc(mtm),
                NSRect::ZERO,
                &configuration,
            );
            view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            view.setUIDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            view.setAllowsLinkPreview(false);
            if view.respondsToSelector(sel!(setInspectable:)) {
                view.setInspectable(false);
            }
            (world, controller, view)
        };

        let container = NativeView::initWithFrame(NativeView::alloc(mtm), NSRect::ZERO);
        container.setHidden(true);
        configure_view(&view, &container, &delegate, options.theme.background);
        container.addSubview(&view);
        host.addSubview(&container);

        let this = Self {
            host,
            container,
            view,
            controller,
            world,
            delegate,
        };
        let html = NSString::from_str(&wrap_html(&options.html, &options.theme));
        let load = unsafe { this.view.loadHTMLString_baseURL(&html, None) }
            .ok_or("WebKit could not load the form HTML")?;
        *this.delegate.ivars().initial_load.borrow_mut() = Some(load);
        Ok(this)
    }

    pub(super) fn place(&mut self, _: &mut Cx, frame: Rect, visible: Option<Rect>, _: f64) {
        let Some(visible) = visible else {
            if !self.container.isHidden() {
                self.blur();
                self.container.setHidden(true);
            }
            return;
        };
        // Makepad's Apple logical pixels are points, not backing-store pixels.
        let clip = native_rect(&self.host, visible);
        if self.container.frame() != clip {
            self.container.setFrame(clip);
        }
        let local = native_rect(
            &self.container,
            Rect {
                pos: frame.pos - visible.pos,
                size: frame.size,
            },
        );
        if self.view.frame() != local {
            self.view.setFrame(local);
        }
        if self.container.isHidden() {
            self.container.setHidden(false);
        }
    }

    pub(super) fn run_script(&mut self, script: &str) {
        unsafe {
            self.view
                .evaluateJavaScript_inFrame_inContentWorld_completionHandler(
                    &NSString::from_str(script),
                    None,
                    &self.world,
                    None,
                );
        }
    }

    pub(super) fn blur(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(window) = self.view.window() {
            if window
                .firstResponder()
                .and_then(|responder| responder.downcast::<NativeView>().ok())
                .is_some_and(|responder| responder.isDescendantOf(&self.view))
            {
                // Makepad routes keys from its NSWindow, not from its render view.
                window.makeFirstResponder(None);
            }
        }
        #[cfg(target_os = "ios")]
        self.view.resignFirstResponder();
    }

    pub(super) fn pump(&mut self, _: &mut Cx) -> Vec<WebViewEvent> {
        std::mem::take(&mut *self.delegate.ivars().events.borrow_mut())
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        self.blur();
        unsafe {
            self.view.setNavigationDelegate(None);
            self.view.setUIDelegate(None);
            self.controller
                .removeScriptMessageHandlerForName_contentWorld(ns_string!("paseo"), &self.world);
            self.view.stopLoading();
        }
        self.view.removeFromSuperview();
        self.container.removeFromSuperview();
    }
}

fn native_rect(parent: &NativeView, rect: Rect) -> NSRect {
    let bounds = parent.bounds();
    let y = rect.pos.y;
    #[cfg(target_os = "macos")]
    let y = if parent.isFlipped() {
        y
    } else {
        bounds.size.height - y - rect.size.y
    };
    NSRect::new(
        NSPoint::new(bounds.origin.x + rect.pos.x, bounds.origin.y + y),
        NSSize::new(rect.size.x, rect.size.y),
    )
}

#[cfg(target_os = "macos")]
fn configure_view(view: &WKWebView, container: &NativeView, _: &Delegate, background: Vec4) {
    container.setWantsLayer(true);
    if let Some(layer) = container.layer() {
        layer.setMasksToBounds(true);
    }
    if container.respondsToSelector(sel!(setClipsToBounds:)) {
        container.setClipsToBounds(true);
    }
    let color = NSColor::colorWithSRGBRed_green_blue_alpha(
        background.x.into(),
        background.y.into(),
        background.z.into(),
        background.w.into(),
    );
    unsafe {
        // Transparent until the HTML paints its own background, including on macOS 13.
        if view.respondsToSelector(sel!(_setDrawsBackground:)) {
            view.setValue_forKey(
                Some(&NSNumber::numberWithBool(false)),
                ns_string!("drawsBackground"),
            );
        }
        view.setUnderPageBackgroundColor(Some(&color));
    }
}

#[cfg(target_os = "ios")]
fn configure_view(view: &WKWebView, container: &NativeView, delegate: &Delegate, background: Vec4) {
    container.setClipsToBounds(true);
    let color = UIColor::colorWithRed_green_blue_alpha(
        background.x.into(),
        background.y.into(),
        background.z.into(),
        background.w.into(),
    );
    container.setBackgroundColor(Some(&color));
    view.setOpaque(false);
    view.setBackgroundColor(Some(&color));

    let scroll = unsafe { view.scrollView() };
    scroll.setScrollEnabled(false);
    scroll.setContentInsetAdjustmentBehavior(UIScrollViewContentInsetAdjustmentBehavior::Never);
    if let Some(pinch) = scroll.pinchGestureRecognizer() {
        pinch.setEnabled(false);
    }
    let pan = unsafe {
        UIPanGestureRecognizer::initWithTarget_action(
            UIPanGestureRecognizer::alloc(delegate.mtm()),
            Some(delegate),
            Some(sel!(paseoPan:)),
        )
    };
    pan.setMaximumNumberOfTouches(1);
    pan.setCancelsTouchesInView(false);
    pan.setDelaysTouchesBegan(false);
    pan.setDelaysTouchesEnded(false);
    pan.setDelegate(Some(ProtocolObject::from_ref(delegate)));
    container.addGestureRecognizer(&pan);
}
