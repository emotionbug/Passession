//! Platforms without a web view backend: callers show their native card.

use super::{WebViewEvent, WebViewOptions};
use makepad_widgets::{Cx, Rect, WindowId};

pub(super) fn supported() -> bool {
    false
}

pub(super) struct WebView;

impl WebView {
    pub(super) fn new(_: &mut Cx, _: WindowId, _: WebViewOptions) -> Result<Self, String> {
        Err("Web views are not available on this platform".into())
    }

    pub(super) fn place(&mut self, _: &mut Cx, _: Rect, _: Option<Rect>, _: f64) {}

    pub(super) fn run_script(&mut self, _: &str) {}

    pub(super) fn blur(&mut self) {}

    pub(super) fn pump(&mut self, _: &mut Cx) -> Vec<WebViewEvent> {
        Vec::new()
    }
}
