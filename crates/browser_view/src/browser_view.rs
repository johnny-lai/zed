use std::time::Instant;

use editor::Editor;
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Bounds, Context, Corners, DispatchPhase, Element, ElementId, Entity,
    EventEmitter, FocusHandle, Focusable, GlobalElementId, Hitbox, HitboxBehavior,
    InspectorElementId, IntoElement, LayoutId, MouseDownEvent, ParentElement, Pixels, Render,
    SharedString, Style, Styled, Subscription, Task, Window, actions, div, relative, size,
};
use settings::{LinkOpenBehavior, RegisterSetting, Settings};
use ui::{Tooltip, prelude::*};
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
    if cfg!(not(target_os = "macos")) {
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
    use gpui::{Bounds, Pixels, Window};
    use util::ResultExt as _;

    use crate::WebViewEvent;

    #[derive(Clone)]
    pub struct NativeWebView(Rc<wry::WebView>);

    pub fn build(
        url: &str,
        window: &Window,
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

#[cfg(not(target_os = "macos"))]
mod webview {
    use futures::channel::mpsc::UnboundedSender;
    use gpui::{Bounds, Pixels, Window};

    use crate::WebViewEvent;

    #[derive(Clone)]
    pub struct NativeWebView;

    pub fn build(
        _url: &str,
        _window: &Window,
        _events: UnboundedSender<WebViewEvent>,
    ) -> anyhow::Result<NativeWebView> {
        anyhow::bail!("the browser pane is only supported on macOS")
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
        let webview = match webview::build(&url, window, events_tx) {
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
        let subscriptions = vec![cx.on_focus(&focus_handle, window, |browser, _, _| {
            if let Some(webview) = &browser.webview {
                webview.focus();
            }
        })];

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
}
