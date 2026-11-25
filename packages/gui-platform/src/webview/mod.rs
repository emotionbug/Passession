//! An OS web view placed over a window's content: the HTML of an OMP `ask` form inside its
//! question card.
//!
//! The page is untrusted: its own scripts never run (a CSP in [`wrap_html`] plus each
//! engine's script settings), it cannot navigate, and it loads nothing but inline styles and
//! `data:`/`http(s):` images. The app injects `bridge.js`, which themes the form, adds the
//! card's Other answers, reports the content height and collects the form data on submit.
//!
//! Every method runs on the UI thread. Engines without a backend report
//! [`WebView::supported`] `false`; the caller then shows its native card instead.

use makepad_widgets::{Cx, Rect, Vec4, WindowId};
use serde_json::{Value, json};

// One backend per platform; each implements `supported()` and `WebView` with `new`,
// `place`, `run_script`, `blur` and `pump` as `linux.rs` does.
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod backend;
#[cfg(target_arch = "wasm32")]
#[path = "web.rs"]
mod backend;
#[cfg(windows)]
#[path = "windows.rs"]
mod backend;
#[cfg(any(target_os = "macos", target_os = "ios"))]
#[path = "apple.rs"]
mod backend;
#[cfg(target_os = "android")]
#[path = "android.rs"]
mod backend;

/// `bridge.js`: defines `window.paseoInit(doc, post, config)`.
pub const BRIDGE: &str = include_str!("bridge.js");

/// The colors and text size the page takes from the card.
#[derive(Clone, Debug, PartialEq)]
pub struct WebTheme {
    pub dark: bool,
    /// The card surface the page is drawn on.
    pub background: Vec4,
    pub foreground: Vec4,
    pub muted: Vec4,
    pub border: Vec4,
    pub accent: Vec4,
    /// Fields and code.
    pub surface: Vec4,
    /// CSS px, equal to the app's logical px.
    pub font_size: f64,
}

#[derive(Clone, Debug)]
pub struct WebViewOptions {
    /// The form's HTML as the agent wrote it (a fragment or a document).
    pub html: String,
    /// Names of radio/checkbox groups that get an Other text answer.
    pub other: Vec<String>,
    /// Placeholder of the Other text field.
    pub other_label: String,
    pub theme: WebTheme,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WebViewEvent {
    /// The content height in logical px.
    Height(f64),
    /// The form passed validation: `{values: {name: [value...]}, other: {name: text}}`.
    Submitted(Value),
    /// Validation failed; the engine shows which field to fix.
    Invalid,
    /// A wheel turn the page does not use, in logical px (positive: toward the end).
    Scroll { dx: f64, dy: f64 },
    /// The engine failed after creation (crashed web process, lost window).
    Failed(String),
}

/// One web view. Dropping it destroys the native view.
pub struct WebView {
    view: backend::WebView,
    window: WindowId,
    /// The last placement asked for: frame, clip and dpi.
    placed: Option<(Rect, Option<Rect>, f64)>,
    /// The caller reports something the app draws over the view ([`WebView::set_obscured`]).
    obscured: bool,
    /// Whether the last placement was applied hidden (window covered or view obscured).
    hidden: bool,
}

thread_local! {
    /// Windows whose content the app currently draws over (menus, dialogs, sheets).
    static COVERED: std::cell::RefCell<Vec<WindowId>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Hides every web view of `window` while the app draws something over its content: a native
/// view would paint above the app's menus, dialogs and sheets. Views reappear in place when
/// it is uncovered. Call from the UI thread on every event.
pub fn set_covered(window: WindowId, covered: bool) {
    COVERED.with(|windows| {
        let mut windows = windows.borrow_mut();
        let position = windows.iter().position(|w| *w == window);
        match (covered, position) {
            (true, None) => windows.push(window),
            (false, Some(index)) => {
                windows.swap_remove(index);
            }
            _ => {}
        }
    });
}

fn is_covered(window: WindowId) -> bool {
    COVERED.with(|windows| windows.borrow().contains(&window))
}

impl WebView {
    /// Whether this platform can show web views now (a backend exists and its engine loads).
    pub fn supported() -> bool {
        backend::supported()
    }

    pub fn new(cx: &mut Cx, window: WindowId, options: WebViewOptions) -> Result<Self, String> {
        Ok(Self {
            view: backend::WebView::new(cx, window, options)?,
            window,
            placed: None,
            obscured: false,
            hidden: false,
        })
    }

    /// Places the view at `frame` (window logical px) and shows the part inside `clip`;
    /// `None` or an empty clip hides it. `dpi` is the window's device pixels per logical px.
    pub fn place(&mut self, cx: &mut Cx, frame: Rect, clip: Option<Rect>, dpi: f64) {
        self.placed = Some((frame, clip, dpi));
        self.apply(cx);
    }

    /// The part of the view on screen: its frame within the clip of the last placement.
    pub fn visible_rect(&self) -> Option<Rect> {
        let (frame, clip, _) = self.placed?;
        clip.map(|clip| intersect(frame, clip))
            .filter(|visible| visible.size.x >= 1.0 && visible.size.y >= 1.0)
    }

    /// Hides the view while the app draws a popover, menu or similar surface over it, and
    /// shows it again in place once `obscured` is false.
    pub fn set_obscured(&mut self, cx: &mut Cx, obscured: bool) {
        if self.obscured != obscured {
            self.obscured = obscured;
            self.apply(cx);
        }
    }

    fn apply(&mut self, cx: &mut Cx) {
        let Some((frame, _, dpi)) = self.placed else {
            return;
        };
        self.hidden = self.obscured || is_covered(self.window);
        let visible = self.visible_rect().filter(|_| !self.hidden);
        self.view.place(cx, frame, visible, dpi);
    }

    /// Validates and collects the form: [`WebViewEvent::Submitted`] or [`WebViewEvent::Invalid`].
    pub fn submit(&mut self) {
        self.view
            .run_script("window.paseo && window.paseo.submit()");
    }

    /// The app took the keyboard back (a press outside the view).
    pub fn blur(&mut self) {
        self.view.blur();
    }

    /// Runs the engine's pending work and returns what the page reported. Call from a
    /// ~16 ms timer while the view exists, and on `Event::Signal`.
    pub fn pump(&mut self, cx: &mut Cx) -> Vec<WebViewEvent> {
        if (self.obscured || is_covered(self.window)) != self.hidden {
            self.apply(cx);
        }
        self.view.pump(cx)
    }
}

/// The page the engine loads: `html` with a CSP that blocks scripts and remote loads other
/// than images, form posts and `<base>` rewrites (`form-action` and `base-uri` do not fall
/// back to `default-src`), and the base style the theme variables drive.
pub fn wrap_html(html: &str, theme: &WebTheme) -> String {
    let background = css_color(theme.background);
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
         img-src data: http: https:; style-src 'unsafe-inline'; font-src data:; \
         form-action 'none'; base-uri 'none'\">\
         <meta name=\"viewport\" content=\"width=device-width\">\
         <style>{STYLE}html{{background:{background}}}</style></head><body>{html}</body></html>"
    )
}

/// Inline style of the form: system fonts at the card's size, fields like the card's.
const STYLE: &str = "\
html,body{margin:0;padding:0;overflow:hidden;color:var(--paseo-fg);\
font:var(--paseo-size) system-ui,-apple-system,'Segoe UI',Roboto,'Noto Sans KR','Apple SD Gothic Neo','Malgun Gothic',sans-serif;\
line-height:1.5;accent-color:var(--paseo-accent);-webkit-text-size-adjust:100%}\
body{padding:0 0 1px}form{margin:0}p{margin:0 0 10px}\
h1,h2,h3,h4{margin:4px 0 10px;line-height:1.3}h1{font-size:1.3em}h2{font-size:1.15em}h3,h4{font-size:1em}\
fieldset{border:1px solid var(--paseo-border);border-radius:8px;margin:0 0 12px;padding:6px 12px 8px}\
legend{color:var(--paseo-muted);padding:0 4px}label{display:block;margin:6px 0}\
input,select,textarea,button{font:inherit;color:inherit}\
input[type=text],input[type=number],input[type=email],input[type=url],input[type=date],input[type=search],\
input[type=password],input[type=tel],input[type=time],input[type=datetime-local],select,textarea{\
box-sizing:border-box;max-width:100%;background:var(--paseo-surface);border:1px solid var(--paseo-border);\
border-radius:6px;padding:5px 8px}textarea{width:100%;min-height:4.5em}\
input:focus,select:focus,textarea:focus{outline:2px solid var(--paseo-accent);outline-offset:-1px}\
input[type=radio],input[type=checkbox]{margin:0 6px 0 0;vertical-align:-2px}\
a{color:var(--paseo-accent)}code,pre,kbd{font-family:ui-monospace,'SF Mono',Menlo,Consolas,monospace;font-size:.92em}\
code{background:var(--paseo-surface);border-radius:4px;padding:1px 4px}\
pre{background:var(--paseo-surface);border-radius:6px;padding:8px 10px;overflow:auto;white-space:pre-wrap}\
pre code{background:none;padding:0}img,svg,video{max-width:100%;height:auto}\
table{border-collapse:collapse;margin:0 0 10px}th,td{border:1px solid var(--paseo-border);padding:4px 8px;text-align:left}\
ul,ol{margin:0 0 10px;padding-left:22px}hr{border:0;border-top:1px solid var(--paseo-border)}\
.paseo-other input[type=text]{width:min(100%,320px)}";

/// The script a native engine injects at document end: `bridge.js`, then `window.paseo`
/// bound to the page with `post` as the transport (a JS expression taking one string).
pub fn bootstrap(options: &WebViewOptions, post: &str) -> String {
    let config = config(options);
    format!(
        "{BRIDGE}\nwindow.paseo = window.paseoInit(document, (message) => ({post})(JSON.stringify(message)), {config});"
    )
}

/// The `config` argument of `paseoInit`.
pub fn config(options: &WebViewOptions) -> Value {
    let theme = &options.theme;
    json!({
        "dark": theme.dark,
        "other": options.other,
        "otherLabel": options.other_label,
        "theme": {
            "--paseo-fg": css_color(theme.foreground),
            "--paseo-muted": css_color(theme.muted),
            "--paseo-border": css_color(theme.border),
            "--paseo-accent": css_color(theme.accent),
            "--paseo-surface": css_color(theme.surface),
            "--paseo-size": format!("{}px", theme.font_size),
        },
    })
}

/// One message the bridge posted (a JSON string), or `None` when it is not one.
pub fn parse_message(message: &str) -> Option<WebViewEvent> {
    let value: Value = serde_json::from_str(message).ok()?;
    match value["type"].as_str()? {
        "height" => value["value"]
            .as_f64()
            .filter(|height| height.is_finite() && *height >= 0.0)
            .map(WebViewEvent::Height),
        "submitted" => Some(WebViewEvent::Submitted(value["data"].clone())),
        "invalid" => Some(WebViewEvent::Invalid),
        "scroll" => {
            let dx = value["dx"].as_f64().filter(|d| d.is_finite())?;
            let dy = value["dy"].as_f64().filter(|d| d.is_finite())?;
            Some(WebViewEvent::Scroll { dx, dy })
        }
        _ => None,
    }
}

pub fn css_color(color: Vec4) -> String {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "rgba({},{},{},{:.3})",
        channel(color.x),
        channel(color.y),
        channel(color.z),
        color.w.clamp(0.0, 1.0)
    )
}

fn intersect(a: Rect, b: Rect) -> Rect {
    let x0 = a.pos.x.max(b.pos.x);
    let y0 = a.pos.y.max(b.pos.y);
    let x1 = (a.pos.x + a.size.x).min(b.pos.x + b.size.x);
    let y1 = (a.pos.y + a.size.y).min(b.pos.y + b.size.y);
    Rect {
        pos: makepad_widgets::dvec2(x0, y0),
        size: makepad_widgets::dvec2((x1 - x0).max(0.0), (y1 - y0).max(0.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_from_the_bridge() {
        assert_eq!(
            parse_message(r#"{"type":"height","value":212}"#),
            Some(WebViewEvent::Height(212.0))
        );
        assert_eq!(
            parse_message(r#"{"type":"submitted","data":{"values":{"a":["1"]},"other":{}}}"#),
            Some(WebViewEvent::Submitted(
                json!({"values":{"a":["1"]},"other":{}})
            ))
        );
        assert_eq!(
            parse_message(r#"{"type":"invalid"}"#),
            Some(WebViewEvent::Invalid)
        );
        assert_eq!(parse_message(r#"{"type":"height","value":-1}"#), None);
        assert_eq!(parse_message("not json"), None);
    }

    #[test]
    fn the_page_blocks_scripts_before_the_form() {
        let theme = WebTheme {
            dark: false,
            background: Vec4 {
                x: 1.0,
                y: 1.0,
                z: 1.0,
                w: 1.0,
            },
            foreground: Vec4::default(),
            muted: Vec4::default(),
            border: Vec4::default(),
            accent: Vec4::default(),
            surface: Vec4::default(),
            font_size: 14.0,
        };
        let page = wrap_html("<form><script>x()</script></form>", &theme);
        let csp = page.find("Content-Security-Policy").unwrap();
        assert!(csp < page.find("<form>").unwrap());
        assert!(page.contains("default-src 'none'"));
        assert!(page.contains("form-action 'none'") && page.contains("base-uri 'none'"));
        assert!(!page.contains("script-src"), "no source may run scripts");
    }
}
