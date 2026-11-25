//! A sandboxed iframe over the Makepad canvas, clipped by a parent div.
//! The iframe cannot run scripts; the trusted bridge operates on its document from the parent.

use super::{BRIDGE, WebViewEvent, WebViewOptions, config, parse_message, wrap_html};
use makepad_widgets::{Cx, Rect, WindowId};
use std::cell::RefCell;
use std::rc::Rc;

pub(super) fn supported() -> bool {
    let supported = paseo_browser::webview_supported();
    if !supported {
        static REPORTED: std::sync::Once = std::sync::Once::new();
        REPORTED.call_once(|| makepad_widgets::log!("Web views unavailable in this browser"));
    }
    supported
}

pub(super) struct WebView {
    view: paseo_browser::WebView,
    events: Rc<RefCell<Vec<WebViewEvent>>>,
}

impl WebView {
    pub(super) fn new(_: &mut Cx, _: WindowId, options: WebViewOptions) -> Result<Self, String> {
        let page = wrap_html("", &options.theme);
        let config = config(&options).to_string();
        let events = Rc::new(RefCell::new(Vec::new()));
        let queue = Rc::clone(&events);
        let view =
            paseo_browser::WebView::open(&page, &options.html, BRIDGE, &config, move |event| {
                let event = match event {
                    Ok(message) => parse_message(message),
                    Err(error) => Some(WebViewEvent::Failed(error.to_owned())),
                };
                if let Some(event) = event {
                    queue.borrow_mut().push(event);
                }
            })?;
        Ok(Self { view, events })
    }

    pub(super) fn place(&mut self, _: &mut Cx, frame: Rect, visible: Option<Rect>, dpi: f64) {
        let rect = |rect: Rect| [rect.pos.x, rect.pos.y, rect.size.x, rect.size.y];
        self.view
            .place(rect(frame), visible.map(rect).unwrap_or_default(), dpi);
    }

    pub(super) fn run_script(&mut self, script: &str) {
        self.view.run_script(script);
    }

    pub(super) fn blur(&mut self) {
        self.view.blur();
    }

    pub(super) fn pump(&mut self, _: &mut Cx) -> Vec<WebViewEvent> {
        std::mem::take(&mut *self.events.borrow_mut())
    }
}
