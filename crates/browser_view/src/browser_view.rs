use std::time::Instant;

use editor::Editor;
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Bounds, Context, Corners, DispatchPhase, Element, ElementId, Entity,
    EventEmitter, FocusHandle, Focusable, GlobalElementId, Hitbox, HitboxBehavior,
    InspectorElementId, IntoElement, LayoutId, MouseDownEvent, ParentElement, Pixels, Render,
    SharedString, Style, Styled, Subscription, Task, Window, actions, div, relative, size,
};
#[cfg(target_os = "linux")]
use gpui::{
    KeyDownEvent, KeyUpEvent, MouseButton, MouseMoveEvent, MouseUpEvent, ScrollDelta,
    ScrollWheelEvent,
};
use settings::{LinkOpenBehavior, RegisterSetting, Settings};
use ui::{Tooltip, prelude::*};
#[cfg(target_os = "linux")]
use util::ResultExt as _;
use workspace::{
    LayoutRole, Pane, Workspace,
    item::{Item, ItemEvent, TabContentParams},
};

actions!(
    browser,
    [
        /// Opens a new browser pane.
        OpenBrowser,
        /// Navigates back in the browser history.
        Back,
        /// Navigates forward in the browser history.
        Forward,
        /// Reloads the current page.
        Reload,
        /// Moves focus to the browser address bar.
        FocusAddressBar
    ]
);

const DEFAULT_URL: &str = "https://www.google.com";

/// The settings for the built-in browser.
#[derive(Clone, Copy, Debug, Default, RegisterSetting)]
pub struct BrowserSettings {
    /// Where links opened from elsewhere in the app (e.g. the terminal) appear.
    pub link_open_behavior: LinkOpenBehavior,
}

impl Settings for BrowserSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let browser = content.browser.clone().unwrap();
        Self {
            link_open_behavior: browser.link_open_behavior.unwrap(),
        }
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &OpenBrowser, window, cx| {
            open_url(workspace, DEFAULT_URL, window, cx);
        });
    })
    .detach();
}

/// Opens a link from elsewhere in the app: http(s) URLs go to a browser pane
/// according to the `browser.link_open_behavior` setting, everything else is
/// handed to the operating system.
pub fn open_link(
    workspace: &mut Workspace,
    url: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if url.starts_with("http://") || url.starts_with("https://") {
        open_url(workspace, url, window, cx);
    } else {
        cx.open_url(url);
    }
}

/// Opens a URL in a browser pane in this workspace, honoring the
/// `browser.link_open_behavior` setting.
pub fn open_url(
    workspace: &mut Workspace,
    url: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !browser_pane_supported(window) {
        cx.open_url(url);
        return;
    }

    let existing = match BrowserSettings::get_global(cx).link_open_behavior {
        LinkOpenBehavior::AlwaysNew => None,
        LinkOpenBehavior::AlwaysReuse => most_recent_browser(workspace, cx),
        LinkOpenBehavior::ReuseIfVisible => {
            most_recent_browser(workspace, cx).filter(|(pane, browser)| {
                pane.read(cx).active_item().map(|item| item.item_id()) == Some(browser.entity_id())
            })
        }
    };

    if let Some((pane, browser)) = existing {
        browser.update(cx, |browser, cx| browser.navigate_to(url, window, cx));
        pane.update(cx, |pane, cx| {
            if let Some(index) = pane.index_for_item(&browser) {
                pane.activate_item(index, true, false, window, cx);
            }
        });
    } else {
        let pane = workspace.pane_for_layout_role(LayoutRole::Editor, window, cx);
        let browser = cx.new(|cx| BrowserView::new(url.to_string(), window, cx));
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(browser), true, true, None, window, cx)
        });
    }
}

/// Whether this window can host an in-app browser pane: macOS always, Linux
/// only under Wayland (the only Linux compositor `webview::build` currently
/// supports embedding a webview under). Everywhere else, links are handed to
/// the system browser instead.
fn browser_pane_supported(window: &Window) -> bool {
    if cfg!(target_os = "macos") {
        return true;
    }
    if cfg!(target_os = "linux") {
        use raw_window_handle::HasDisplayHandle as _;
        return matches!(
            window.display_handle().map(|handle| handle.as_raw()),
            Ok(raw_window_handle::RawDisplayHandle::Wayland(_))
        );
    }
    false
}

fn most_recent_browser(
    workspace: &Workspace,
    cx: &App,
) -> Option<(Entity<Pane>, Entity<BrowserView>)> {
    let mut result: Option<(Entity<Pane>, Entity<BrowserView>, Instant)> = None;
    for pane in workspace.panes() {
        let browsers = pane.read(cx).items_of_type::<BrowserView>().collect::<Vec<_>>();
        for browser in browsers {
            let last_used_at = browser.read(cx).last_used_at;
            if result
                .as_ref()
                .is_none_or(|(_, _, best)| last_used_at > *best)
            {
                result = Some((pane.clone(), browser, last_used_at));
            }
        }
    }
    result.map(|(pane, browser, _)| (pane, browser))
}

fn normalize_url_input(input: &str) -> String {
    if input.contains("://") {
        input.to_string()
    } else if !input.contains(' ')
        && (input.starts_with("localhost") || input.contains('.'))
    {
        format!("https://{input}")
    } else {
        format!(
            "https://www.google.com/search?q={}",
            urlencoding::encode(input)
        )
    }
}

pub enum WebViewEvent {
    TitleChanged(String),
    UrlChanged(String),
    /// A page requested a new window (e.g. `target="_blank"`); we open it in
    /// the same webview instead.
    OpenUrlInPage(String),
}

#[cfg(target_os = "macos")]
mod webview {
    use std::rc::Rc;

    use anyhow::Context as _;
    use futures::channel::mpsc::UnboundedSender;
    use gpui::{Bounds, Context as GpuiContext, Pixels, Window};
    use util::ResultExt as _;

    use crate::{BrowserView, WebViewEvent};

    #[derive(Clone)]
    pub struct NativeWebView(Rc<wry::WebView>);

    pub fn build(
        url: &str,
        window: &Window,
        _cx: &mut GpuiContext<BrowserView>,
        events: UnboundedSender<WebViewEvent>,
    ) -> anyhow::Result<NativeWebView> {
        let webview = wry::WebViewBuilder::new()
            .with_url(url)
            .with_visible(false)
            .with_devtools(true)
            .with_navigation_handler({
                let events = events.clone();
                move |url| {
                    events.unbounded_send(WebViewEvent::UrlChanged(url)).ok();
                    true
                }
            })
            .with_on_page_load_handler({
                let events = events.clone();
                move |_event, url| {
                    events.unbounded_send(WebViewEvent::UrlChanged(url)).ok();
                }
            })
            .with_document_title_changed_handler({
                let events = events.clone();
                move |title| {
                    events.unbounded_send(WebViewEvent::TitleChanged(title)).ok();
                }
            })
            .with_new_window_req_handler({
                move |url, _features| {
                    events.unbounded_send(WebViewEvent::OpenUrlInPage(url)).ok();
                    wry::NewWindowResponse::Deny
                }
            })
            // Embedded behind GPUI's rendering surface rather than in front of
            // it, so that modals, menus and tooltips can paint over the page.
            // `BrowserView` punches a cutout to reveal it.
            .build_as_child(
                &window
                    .native_view_container()
                    .context("this window does not support embedding native views")?,
            )?;
        Ok(NativeWebView(Rc::new(webview)))
    }

    impl NativeWebView {
        pub fn set_bounds(&self, bounds: Bounds<Pixels>) {
            self.0
                .set_bounds(wry::Rect {
                    position: wry::dpi::LogicalPosition::new(
                        f64::from(bounds.origin.x),
                        f64::from(bounds.origin.y),
                    )
                    .into(),
                    size: wry::dpi::LogicalSize::new(
                        f64::from(bounds.size.width),
                        f64::from(bounds.size.height),
                    )
                    .into(),
                })
                .log_err();
        }

        pub fn set_visible(&self, visible: bool) {
            self.0.set_visible(visible).log_err();
        }

        pub fn load_url(&self, url: &str) {
            self.0.load_url(url).log_err();
        }

        pub fn back(&self) {
            self.0.evaluate_script("history.back();").log_err();
        }

        pub fn forward(&self) {
            self.0.evaluate_script("history.forward();").log_err();
        }

        pub fn reload(&self) {
            self.0.reload().log_err();
        }

        pub fn focus(&self) {
            self.0.focus().log_err();
        }
    }
}

// Linux has no equivalent of macOS's native-view embedding: `wry`'s Linux
// backend needs GTK, and GTK has no supported way to place a foreign toolkit's
// widget inside a plain Wayland surface the way an `NSView` can be added as a
// subview. Instead, the page is rendered off-screen (`GtkOffscreenWindow`,
// GTK's own documented mechanism for rendering a widget subtree that was never
// put on screen) and captured frames are painted through GPUI's ordinary
// image/atlas pipeline like any other bitmap, with input events forwarded in
// as synthetic GDK events -- the same shape `terminal_view` uses to forward
// input into its embedded terminal rather than relying on the OS to deliver it
// to a real child view. Validated end-to-end (rendering, click, and keyboard
// forwarding) against a standalone spike under a real Wayland session before
// writing this.
#[cfg(target_os = "linux")]
mod webview {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Once};
    use std::time::Duration;

    use futures::channel::mpsc::UnboundedSender;
    use gdk::glib::translate::*;
    use gpui::{Bounds, Context as GpuiContext, Pixels, RenderImage, Window};
    use gtk::prelude::*;
    use image::{Frame, RgbaImage};
    use webkit2gtk::{URIRequestExt as _, WebViewExt as _};

    use crate::{BrowserView, WebViewEvent};

    struct Inner {
        offscreen: gtk::OffscreenWindow,
        web_view: webkit2gtk::WebView,
        latest_frame: RefCell<Option<Arc<RenderImage>>>,
    }

    #[derive(Clone)]
    pub struct NativeWebView(Rc<Inner>);

    /// Starts GTK once per process and keeps its main loop pumped for as long
    /// as the process runs, since GPUI has no event loop of its own that GTK
    /// can be integrated into. Runs as a plain poll loop rather than being
    /// woken by GLib's own file descriptors -- simpler, at the cost of a
    /// little latency/overhead while any browser pane exists.
    fn ensure_gtk_running(cx: &mut GpuiContext<BrowserView>) {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            // WebKitGTK aborts trying to create a GL context when hosted in a
            // GtkOffscreenWindow, since there is no on-screen compositor
            // surface for it to accelerate against; its non-composited path
            // avoids that (confirmed against a real Wayland session).
            // SAFETY: called once, at the very first browser pane creation,
            // before any other code in this process would plausibly read
            // this variable.
            unsafe {
                std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
            }
            gtk::init().expect("failed to initialize GTK for the browser pane");

            cx.spawn(async move |_this, cx| {
                loop {
                    while gtk::events_pending() {
                        gtk::main_iteration_do(false);
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(16))
                        .await;
                }
            })
            .detach();
        });
    }

    /// Copies a captured frame out of a cairo surface into a `RenderImage`.
    ///
    /// Cairo's `ARGB32` format is premultiplied alpha stored as B, G, R, A
    /// bytes in memory on little-endian platforms -- already the byte order
    /// `RenderImage` expects, but it wants straight (unpremultiplied) alpha.
    fn capture_frame(source: cairo::ImageSurface) -> Option<RenderImage> {
        let width = source.width();
        let height = source.height();
        if width <= 0 || height <= 0 {
            return None;
        }

        // `source` is the offscreen window's own surface, which GTK also
        // holds a reference to -- `ImageSurface::data()` refuses to hand out
        // a mutable borrow while the cairo-level refcount is above 1. Paint
        // it onto a fresh surface we exclusively own instead of trying to
        // read `source` directly.
        let mut copy = cairo::ImageSurface::create(cairo::Format::ARgb32, width, height).ok()?;
        {
            let context = cairo::Context::new(&copy).ok()?;
            context.set_source_surface(&source, 0.0, 0.0).ok()?;
            context.paint().ok()?;
        }

        let stride = copy.stride() as usize;
        let row_bytes = width as usize * 4;

        let mut packed = vec![0u8; row_bytes * height as usize];
        {
            let data = copy.data().ok()?;
            for row in 0..height as usize {
                let src = &data[row * stride..row * stride + row_bytes];
                let dst = &mut packed[row * row_bytes..(row + 1) * row_bytes];
                dst.copy_from_slice(src);
            }
        }

        for pixel in packed.chunks_exact_mut(4) {
            let alpha = pixel[3];
            if alpha > 0 && alpha < 255 {
                let alpha = alpha as f32 / 255.0;
                pixel[0] = (pixel[0] as f32 / alpha).min(255.0) as u8;
                pixel[1] = (pixel[1] as f32 / alpha).min(255.0) as u8;
                pixel[2] = (pixel[2] as f32 / alpha).min(255.0) as u8;
            }
        }

        let buffer = RgbaImage::from_raw(width as u32, height as u32, packed)?;
        Some(RenderImage::new(smallvec::smallvec![Frame::new(buffer)]))
    }

    pub fn build(
        url: &str,
        _window: &Window,
        cx: &mut GpuiContext<BrowserView>,
        events: UnboundedSender<WebViewEvent>,
    ) -> anyhow::Result<NativeWebView> {
        ensure_gtk_running(cx);

        let offscreen = gtk::OffscreenWindow::new();
        let fixed = gtk::Fixed::new();
        offscreen.add(&fixed);

        let web_view = webkit2gtk::WebView::new();
        fixed.put(&web_view, 0, 0);
        offscreen.set_default_size(1, 1);
        offscreen.show_all();

        web_view.load_uri(url);

        {
            let events = events.clone();
            web_view.connect_title_notify(move |web_view| {
                let title = web_view.title().map(|title| title.to_string());
                events
                    .unbounded_send(WebViewEvent::TitleChanged(title.unwrap_or_default()))
                    .ok();
            });
        }
        {
            let events = events.clone();
            web_view.connect_uri_notify(move |web_view| {
                if let Some(uri) = web_view.uri() {
                    events
                        .unbounded_send(WebViewEvent::UrlChanged(uri.to_string()))
                        .ok();
                }
            });
        }
        {
            // A page requested a new window (e.g. `target="_blank"`); mirrors
            // the macOS `with_new_window_req_handler` behavior of opening it
            // in the same webview rather than a real new window.
            web_view.connect_create(move |_web_view, navigation_action| {
                if let Some(uri) = navigation_action.request().and_then(|request| request.uri()) {
                    events
                        .unbounded_send(WebViewEvent::OpenUrlInPage(uri.to_string()))
                        .ok();
                }
                None
            });
        }

        let inner = Rc::new(Inner {
            offscreen: offscreen.clone(),
            web_view,
            latest_frame: RefCell::new(None),
        });

        {
            let inner = inner.clone();
            offscreen.connect_damage_event(move |window, _event| {
                if let Some(surface) = window.surface() {
                    if let Ok(image_surface) = cairo::ImageSurface::try_from(surface) {
                        match capture_frame(image_surface) {
                            Some(frame) => {
                                inner.latest_frame.replace(Some(Arc::new(frame)));
                            }
                            None => log::debug!("browser_view: failed to capture a webview frame"),
                        }
                    }
                }
                false
            });
        }

        Ok(NativeWebView(inner))
    }

    impl NativeWebView {
        pub fn set_bounds(&self, bounds: Bounds<Pixels>) {
            let width = f32::from(bounds.size.width).round().max(1.0) as i32;
            let height = f32::from(bounds.size.height).round().max(1.0) as i32;
            self.0.web_view.set_size_request(width, height);
            self.0.offscreen.resize(width, height);
        }

        pub fn set_visible(&self, visible: bool) {
            // There is no native on-screen surface to hide -- offscreen
            // rendering keeps happening either way -- but hiding the widget
            // itself suppresses unnecessary internal repaint/compositing work
            // for a backgrounded tab while keeping the last captured frame
            // around for an instant reappearance when it's shown again.
            self.0.web_view.set_visible(visible);
        }

        pub fn load_url(&self, url: &str) {
            self.0.web_view.load_uri(url);
        }

        pub fn back(&self) {
            self.0.web_view.go_back();
        }

        pub fn forward(&self) {
            self.0.web_view.go_forward();
        }

        pub fn reload(&self) {
            self.0.web_view.reload();
        }

        pub fn focus(&self) {
            self.0.web_view.grab_focus();
        }

        pub fn latest_frame(&self) -> Option<Arc<RenderImage>> {
            self.0.latest_frame.borrow().clone()
        }

        fn pointer_device() -> Option<gdk::Device> {
            gdk::Display::default()?.default_seat()?.pointer()
        }

        fn keyboard_device() -> Option<gdk::Device> {
            gdk::Display::default()?.default_seat()?.keyboard()
        }

        pub fn dispatch_pointer_button(
            &self,
            event_type: gdk::EventType,
            position: gpui::Point<Pixels>,
            button: u32,
        ) {
            let Some(window) = self.0.web_view.window() else {
                return;
            };
            let mut event = gdk::Event::new(event_type);
            // SAFETY: `event` was just created as this exact variant, so
            // reinterpreting its raw pointer as `GdkEventButton` is valid; the
            // fields written below are exactly those C's `GdkEventButton`
            // declares, and `window` is given its own owned reference via
            // `to_glib_full` to match what freeing the event will release.
            unsafe {
                let raw: *mut gdk_sys::GdkEvent = event.to_glib_none_mut().0;
                let button_event = raw as *mut gdk_sys::GdkEventButton;
                (*button_event).window = window.to_glib_full();
                (*button_event).send_event = 1;
                (*button_event).time = gdk_sys::GDK_CURRENT_TIME as u32;
                (*button_event).x = f64::from(position.x);
                (*button_event).y = f64::from(position.y);
                (*button_event).axes = std::ptr::null_mut();
                (*button_event).state = 0;
                (*button_event).button = button;
                (*button_event).x_root = f64::from(position.x);
                (*button_event).y_root = f64::from(position.y);
            }
            event.set_device(Self::pointer_device().as_ref());
            gtk::main_do_event(&mut event);
        }

        pub fn dispatch_pointer_motion(&self, position: gpui::Point<Pixels>) {
            let Some(window) = self.0.web_view.window() else {
                return;
            };
            let mut event = gdk::Event::new(gdk::EventType::MotionNotify);
            // SAFETY: see `dispatch_pointer_button`; layout matches `GdkEventMotion`.
            unsafe {
                let raw: *mut gdk_sys::GdkEvent = event.to_glib_none_mut().0;
                let motion_event = raw as *mut gdk_sys::GdkEventMotion;
                (*motion_event).window = window.to_glib_full();
                (*motion_event).send_event = 1;
                (*motion_event).time = gdk_sys::GDK_CURRENT_TIME as u32;
                (*motion_event).x = f64::from(position.x);
                (*motion_event).y = f64::from(position.y);
                (*motion_event).axes = std::ptr::null_mut();
                (*motion_event).state = 0;
                (*motion_event).is_hint = 0;
                (*motion_event).x_root = f64::from(position.x);
                (*motion_event).y_root = f64::from(position.y);
            }
            event.set_device(Self::pointer_device().as_ref());
            gtk::main_do_event(&mut event);
        }

        pub fn dispatch_scroll(&self, position: gpui::Point<Pixels>, delta_x: f64, delta_y: f64) {
            let Some(window) = self.0.web_view.window() else {
                return;
            };
            let mut event = gdk::Event::new(gdk::EventType::Scroll);
            // SAFETY: see `dispatch_pointer_button`; layout matches `GdkEventScroll`.
            unsafe {
                let raw: *mut gdk_sys::GdkEvent = event.to_glib_none_mut().0;
                let scroll_event = raw as *mut gdk_sys::GdkEventScroll;
                (*scroll_event).window = window.to_glib_full();
                (*scroll_event).send_event = 1;
                (*scroll_event).time = gdk_sys::GDK_CURRENT_TIME as u32;
                (*scroll_event).x = f64::from(position.x);
                (*scroll_event).y = f64::from(position.y);
                (*scroll_event).state = 0;
                (*scroll_event).direction = gdk_sys::GDK_SCROLL_SMOOTH;
                (*scroll_event).x_root = f64::from(position.x);
                (*scroll_event).y_root = f64::from(position.y);
                (*scroll_event).delta_x = delta_x;
                (*scroll_event).delta_y = delta_y;
                (*scroll_event).is_stop = 0;
            }
            event.set_device(Self::pointer_device().as_ref());
            gtk::main_do_event(&mut event);
        }

        pub fn dispatch_key(&self, event_type: gdk::EventType, keyval: u32) {
            let Some(window) = self.0.web_view.window() else {
                return;
            };
            let mut event = gdk::Event::new(event_type);
            // SAFETY: see `dispatch_pointer_button`; layout matches `GdkEventKey`.
            unsafe {
                let raw: *mut gdk_sys::GdkEvent = event.to_glib_none_mut().0;
                let key_event = raw as *mut gdk_sys::GdkEventKey;
                (*key_event).window = window.to_glib_full();
                (*key_event).send_event = 1;
                (*key_event).time = gdk_sys::GDK_CURRENT_TIME as u32;
                (*key_event).state = 0;
                (*key_event).keyval = keyval;
                (*key_event).length = 0;
                (*key_event).string = std::ptr::null_mut();
                (*key_event).hardware_keycode = 0;
                (*key_event).group = 0;
                (*key_event).is_modifier = 0;
            }
            event.set_device(Self::keyboard_device().as_ref());
            gtk::main_do_event(&mut event);
        }
    }

    /// Maps a GPUI key name (`Keystroke::key`, e.g. `"a"`, `"enter"`,
    /// `"backspace"`) to a GDK keyval, for forwarding raw keystrokes into the
    /// offscreen webview. There is no IME/text-composition bridging here --
    /// this covers plain keys well enough for address bars and simple page
    /// forms, not complex-script input.
    pub fn keyval_for_key(key: &str) -> Option<u32> {
        let name = match key {
            "enter" => "Return",
            "backspace" => "BackSpace",
            "delete" => "Delete",
            "tab" => "Tab",
            "escape" => "Escape",
            "space" => "space",
            "up" => "Up",
            "down" => "Down",
            "left" => "Left",
            "right" => "Right",
            "home" => "Home",
            "end" => "End",
            "pageup" => "Page_Up",
            "pagedown" => "Page_Down",
            _ => {
                let mut chars = key.chars();
                let (Some(char), None) = (chars.next(), chars.next()) else {
                    return None;
                };
                return Some(*gdk::keys::Key::from_unicode(char));
            }
        };
        Some(*gdk::keys::Key::from_name(name))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod webview {
    use futures::channel::mpsc::UnboundedSender;
    use gpui::{Bounds, Context as GpuiContext, Pixels, Window};

    use crate::{BrowserView, WebViewEvent};

    #[derive(Clone)]
    pub struct NativeWebView;

    pub fn build(
        _url: &str,
        _window: &Window,
        _cx: &mut GpuiContext<BrowserView>,
        _events: UnboundedSender<WebViewEvent>,
    ) -> anyhow::Result<NativeWebView> {
        anyhow::bail!("the browser pane is only supported on macOS and Linux/Wayland")
    }

    impl NativeWebView {
        pub fn set_bounds(&self, _bounds: Bounds<Pixels>) {}
        pub fn set_visible(&self, _visible: bool) {}
        pub fn load_url(&self, _url: &str) {}
        pub fn back(&self) {}
        pub fn forward(&self) {}
        pub fn reload(&self) {}
        pub fn focus(&self) {}
    }
}

pub enum BrowserEvent {
    TitleChanged,
}

pub struct BrowserView {
    focus_handle: FocusHandle,
    address_editor: Entity<Editor>,
    webview: Option<webview::NativeWebView>,
    title: Option<SharedString>,
    current_url: String,
    last_used_at: Instant,
    error: Option<SharedString>,
    _event_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
    /// The atlas-backed image most recently painted for this browser's captured
    /// webview frame, kept so the previous frame's atlas tile can be freed once
    /// a newer one is painted in its place. Unused on macOS, where the page is a
    /// real native view rather than a captured frame.
    #[cfg(target_os = "linux")]
    last_painted_frame: Option<std::sync::Arc<gpui::RenderImage>>,
}

impl BrowserView {
    pub fn new(
        url: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let address_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Enter address", window, cx);
            editor.set_text(url.as_str(), window, cx);
            editor
        });

        let (events_tx, mut events_rx) = futures::channel::mpsc::unbounded();
        let mut error = None;
        let webview = match webview::build(&url, window, cx, events_tx) {
            Ok(webview) => Some(webview),
            Err(build_error) => {
                log::error!("failed to create webview: {build_error:#}");
                error = Some(format!("Failed to create webview: {build_error}").into());
                None
            }
        };

        let event_task = webview.is_some().then(|| {
            cx.spawn_in(window, async move |this, cx| {
                while let Some(event) = events_rx.next().await {
                    let handled = this.update_in(cx, |browser, window, cx| {
                        browser.handle_webview_event(event, window, cx)
                    });
                    if handled.is_err() {
                        break;
                    }
                }
            })
        });

        let focus_handle = cx.focus_handle();
        let subscriptions = vec![
            cx.on_focus(&focus_handle, window, |browser, _, _| {
                if let Some(webview) = &browser.webview {
                    webview.focus();
                }
            }),
            // GPUI's own focus can move away (e.g. a modal like the file
            // finder opening via a keybinding) without any mouse event for
            // `WebViewElement` to observe, so the webview would otherwise
            // keep OS keyboard focus indefinitely.
            cx.on_blur(&focus_handle, window, |browser, window, _cx| {
                if browser.webview.is_some() {
                    window.reclaim_native_focus();
                }
            }),
        ];

        Self {
            focus_handle,
            address_editor,
            webview,
            title: None,
            current_url: url,
            last_used_at: Instant::now(),
            error,
            _event_task: event_task,
            _subscriptions: subscriptions,
            #[cfg(target_os = "linux")]
            last_painted_frame: None,
        }
    }

    pub fn navigate_to(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.current_url = url.to_string();
        self.last_used_at = Instant::now();
        if let Some(webview) = &self.webview {
            webview.load_url(url);
        }
        self.address_editor
            .update(cx, |editor, cx| editor.set_text(url, window, cx));
        cx.emit(BrowserEvent::TitleChanged);
        cx.notify();
    }

    fn handle_webview_event(
        &mut self,
        event: WebViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            WebViewEvent::TitleChanged(title) => {
                self.title = (!title.is_empty()).then(|| title.into());
                cx.emit(BrowserEvent::TitleChanged);
                cx.notify();
            }
            WebViewEvent::UrlChanged(url) => {
                self.last_used_at = Instant::now();
                if url != self.current_url {
                    self.current_url = url.clone();
                    cx.emit(BrowserEvent::TitleChanged);
                }
                if !self.address_editor.focus_handle(cx).is_focused(window)
                    && self.address_editor.read(cx).text(cx) != url
                {
                    self.address_editor
                        .update(cx, |editor, cx| editor.set_text(url, window, cx));
                }
                cx.notify();
            }
            WebViewEvent::OpenUrlInPage(url) => self.navigate_to(&url, window, cx),
        }
    }

    /// Hides the page. An inactive tab paints no cutout, so its page is already
    /// covered, but it must still be hidden or it would show through the cutout
    /// of *another* browser tab stacked in the same container.
    fn hide_webview(&self, window: &Window) {
        if let Some(webview) = &self.webview {
            window.reclaim_native_focus();
            webview.set_visible(false);
        }
    }

    fn confirm_address(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.address_editor.read(cx).text(cx).trim().to_string();
        if input.is_empty() {
            return;
        }
        self.navigate_to(&normalize_url_input(&input), window, cx);
        if let Some(webview) = &self.webview {
            webview.focus();
        }
    }

    fn focus_address_bar(
        &mut self,
        _: &FocusAddressBar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.address_editor.update(cx, |editor, cx| {
            editor.select_all(&editor::actions::SelectAll, window, cx);
        });
        window.focus(&self.address_editor.focus_handle(cx), cx);
    }

    fn go_back(&mut self, _: &Back, _window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(webview) = &self.webview {
            webview.back();
        }
    }

    fn go_forward(&mut self, _: &Forward, _window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(webview) = &self.webview {
            webview.forward();
        }
    }

    fn reload(&mut self, _: &Reload, _window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(webview) = &self.webview {
            webview.reload();
        }
    }
}

impl Focusable for BrowserView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<BrowserEvent> for BrowserView {}

impl Item for BrowserView {
    type Event = BrowserEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            BrowserEvent::TitleChanged => f(ItemEvent::UpdateTab),
        }
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        if let Some(title) = &self.title {
            title.clone()
        } else if self.current_url.is_empty() {
            "Browser".into()
        } else {
            self.current_url.clone().into()
        }
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(params.text_color())
            .into_any_element()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Public))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.current_url.clone().into())
    }

    fn deactivated(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        self.hide_webview(window);
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Browser Opened")
    }
}

impl Render for BrowserView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        v_flex()
            .key_context("BrowserView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .on_action(cx.listener(Self::confirm_address))
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::reload))
            .on_action(cx.listener(Self::focus_address_bar))
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .p_1()
                    .border_b_1()
                    .border_color(colors.border)
                    .bg(colors.toolbar_background)
                    .child(
                        IconButton::new("browser-back", IconName::ArrowLeft)
                            .icon_size(IconSize::Small)
                            .tooltip(|_window, cx| Tooltip::for_action("Back", &Back, cx))
                            .on_click(cx.listener(|browser, _, window, cx| {
                                browser.go_back(&Back, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new("browser-forward", IconName::ArrowRight)
                            .icon_size(IconSize::Small)
                            .tooltip(|_window, cx| Tooltip::for_action("Forward", &Forward, cx))
                            .on_click(cx.listener(|browser, _, window, cx| {
                                browser.go_forward(&Forward, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new("browser-reload", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(|_window, cx| Tooltip::for_action("Reload", &Reload, cx))
                            .on_click(cx.listener(|browser, _, window, cx| {
                                browser.reload(&Reload, window, cx)
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border_variant)
                            .bg(colors.editor_background)
                            .child(self.address_editor.clone()),
                    ),
            )
            .child(div().flex_1().min_h_0().w_full().map(|content| {
                if self.webview.is_some() {
                    content.child(WebViewElement {
                        browser: cx.entity(),
                    })
                } else {
                    content.flex().items_center().justify_center().child(
                        Label::new(
                            self.error
                                .clone()
                                .unwrap_or_else(|| "Browser is unavailable".into()),
                        )
                        .color(Color::Muted),
                    )
                }
            }))
    }
}

/// Positions the native webview over the bounds this element is laid out at.
struct WebViewElement {
    browser: Entity<BrowserView>,
}

impl IntoElement for WebViewElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for WebViewElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (
            window.request_layout(
                Style {
                    size: size(relative(1.).into(), relative(1.).into()),
                    ..Default::default()
                },
                [],
                cx,
            ),
            (),
        )
    }

    #[cfg(not(target_os = "linux"))]
    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let webview = self.browser.read(cx).webview.clone()?;
        webview.set_bounds(bounds);
        webview.set_visible(true);
        Some(window.insert_hitbox(bounds, HitboxBehavior::NativeView))
    }

    #[cfg(not(target_os = "linux"))]
    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if hitbox.is_none() {
            return;
        }

        // Clears the rendering surface so the page behind it shows through.
        // Anything painted after this — modals, menus, tooltips — paints over
        // the cutout and so appears above the page.
        window.paint_cutout(bounds, Corners::default());

        let has_webview = self.browser.read(cx).webview.is_some();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, _cx| {
            if phase == DispatchPhase::Bubble && has_webview && !bounds.contains(&event.position) {
                // Clicking outside the page returns keyboard focus to GPUI.
                window.reclaim_native_focus();
            }
        });
    }

    // Linux has no native view to embed behind the rendering surface, so the
    // captured webview frame is painted as an ordinary bitmap and input is
    // forwarded in manually, rather than relying on `paint_cutout` +
    // OS-delivered input the way the native-embedding platforms above do.
    #[cfg(target_os = "linux")]
    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let webview = self.browser.read(cx).webview.clone()?;
        webview.set_bounds(bounds);
        webview.set_visible(true);
        Some(window.insert_hitbox(bounds, HitboxBehavior::Normal))
    }

    #[cfg(target_os = "linux")]
    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(hitbox) = hitbox.clone() else {
            return;
        };

        let webview = self.browser.read(cx).webview.clone();
        let frame = webview.as_ref().and_then(|webview| webview.latest_frame());

        if let Some(frame) = frame {
            if window
                .paint_image(bounds, Corners::default(), frame.clone(), 0, false)
                .log_err()
                .is_some()
            {
                let previous = self
                    .browser
                    .update(cx, |browser, _cx| browser.last_painted_frame.replace(frame.clone()));
                if let Some(previous) = previous {
                    if previous.id != frame.id {
                        window.drop_image(previous).log_err();
                    }
                }
            }
        }

        let focus_handle = self.browser.read(cx).focus_handle.clone();

        let Some(webview) = webview else {
            return;
        };

        window.on_mouse_event({
            let webview = webview.clone();
            let hitbox = hitbox.clone();
            let focus_handle = focus_handle.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                // Clicking the page is the only way to give this pane GPUI
                // keyboard focus -- there is no real native view for the OS
                // to hand focus to the way there is on macOS.
                window.focus(&focus_handle, cx);
                if let Some(button) = gdk_button_for(event.button) {
                    webview.dispatch_pointer_button(
                        gdk::EventType::ButtonPress,
                        event.position - bounds.origin,
                        button,
                    );
                }
            }
        });
        window.on_mouse_event({
            let webview = webview.clone();
            let hitbox = hitbox.clone();
            move |event: &MouseUpEvent, phase, window, _cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                if let Some(button) = gdk_button_for(event.button) {
                    webview.dispatch_pointer_button(
                        gdk::EventType::ButtonRelease,
                        event.position - bounds.origin,
                        button,
                    );
                }
            }
        });
        window.on_mouse_event({
            let webview = webview.clone();
            let hitbox = hitbox.clone();
            move |event: &MouseMoveEvent, phase, window, _cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                webview.dispatch_pointer_motion(event.position - bounds.origin);
            }
        });
        window.on_mouse_event({
            let webview = webview.clone();
            move |event: &ScrollWheelEvent, phase, window, _cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                let (delta_x, delta_y) = match event.delta {
                    ScrollDelta::Lines(delta) => (delta.x as f64, delta.y as f64),
                    // GDK's smooth-scroll deltas are roughly in "lines", not
                    // pixels; approximate using a typical line height.
                    ScrollDelta::Pixels(delta) => {
                        (f64::from(delta.x) / 20.0, f64::from(delta.y) / 20.0)
                    }
                };
                webview.dispatch_scroll(event.position - bounds.origin, -delta_x, -delta_y);
            }
        });
        window.on_key_event({
            let webview = webview.clone();
            let focus_handle = focus_handle.clone();
            move |event: &KeyDownEvent, phase, window, _cx| {
                if phase != DispatchPhase::Bubble || !focus_handle.is_focused(window) {
                    return;
                }
                let key = event.keystroke.key_char.as_deref().unwrap_or(&event.keystroke.key);
                if let Some(keyval) = webview::keyval_for_key(key) {
                    webview.dispatch_key(gdk::EventType::KeyPress, keyval);
                }
            }
        });
        window.on_key_event({
            move |event: &KeyUpEvent, phase, window, _cx| {
                if phase != DispatchPhase::Bubble || !focus_handle.is_focused(window) {
                    return;
                }
                let key = event.keystroke.key_char.as_deref().unwrap_or(&event.keystroke.key);
                if let Some(keyval) = webview::keyval_for_key(key) {
                    webview.dispatch_key(gdk::EventType::KeyRelease, keyval);
                }
            }
        });
    }
}

#[cfg(target_os = "linux")]
fn gdk_button_for(button: MouseButton) -> Option<u32> {
    match button {
        MouseButton::Left => Some(1),
        MouseButton::Middle => Some(2),
        MouseButton::Right => Some(3),
        MouseButton::Navigate(_) => None,
    }
}
