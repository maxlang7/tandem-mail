use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    os::raw::c_ulong,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, ensure};
use eframe::egui::{self, Color32, Context, Id, Rect, Ui, scroll_area};
use gtk::{gdk, glib, prelude::*};
use javascriptcore::ValueExt;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use reqwest::Url;
use webkit2gtk::{
    ContextMenuAction, ContextMenuExt, ContextMenuItemExt, LoadEvent, NavigationPolicyDecision,
    NavigationPolicyDecisionExt, NavigationType, PolicyDecisionExt, PolicyDecisionType,
    SettingsExt, URIRequestExt, UserContentInjectedFrames, UserContentManagerExt, UserStyleLevel,
    UserStyleSheet, WebContext, WebView, WebViewExt,
};
use x11_dl::xlib;

use crate::{html_mail::Document, message_body::web_url, ui::Theme};

type MessageKey = (String, String);
fn bridge_id() -> Id {
    Id::new("web-reader")
}
const WORLD: &str = "tandem-reader";

#[derive(Clone, Copy)]
enum Layout {
    Loading,
    Ready(f32),
    Failed,
}

#[derive(Default, Clone)]
struct FrameState {
    layouts: HashMap<MessageKey, Layout>,
    bodies: Vec<Body>,
    scroll: Option<ScrollArea>,
}

#[derive(Clone)]
struct Body {
    id: MessageKey,
    document: Arc<Document>,
    theme: Theme,
    original_colors: bool,
    rect: Rect,
    clip: Rect,
}

#[derive(Clone, Copy)]
struct ScrollArea {
    id: Id,
    state: scroll_area::State,
    max_offset: f32,
    clip: Rect,
}

pub enum Display {
    Shown,
    Failed,
    Unavailable,
}

pub fn display(
    ui: &mut Ui,
    id: &MessageKey,
    document: Arc<Document>,
    theme: Theme,
    original_colors: bool,
) -> Display {
    let Some(layout) = ui.ctx().data(|data| {
        data.get_temp::<FrameState>(bridge_id())
            .map(|frame| frame.layouts.get(id).copied().unwrap_or(Layout::Loading))
    }) else {
        return Display::Unavailable;
    };
    let height = match layout {
        Layout::Loading => 120.,
        Layout::Ready(height) => height,
        Layout::Failed => return Display::Failed,
    };
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    if matches!(layout, Layout::Loading) && ui.is_rect_visible(rect) {
        egui::Spinner::new().paint_at(
            ui,
            Rect::from_center_size(rect.center(), egui::vec2(20., 20.)),
        );
    }
    ui.ctx().data_mut(|data| {
        data.get_temp_mut_or_default::<FrameState>(bridge_id())
            .bodies
            .push(Body {
                id: id.clone(),
                document,
                theme,
                original_colors,
                rect,
                clip: ui.clip_rect(),
            });
    });
    Display::Shown
}

pub fn register_scroll_area(ctx: &Context, area: &scroll_area::ScrollAreaOutput<()>) {
    ctx.data_mut(|data| {
        if let Some(mut frame) = data.get_temp::<FrameState>(bridge_id()) {
            frame.scroll = Some(ScrollArea {
                id: area.id,
                state: area.state,
                max_offset: area.content_size.y - area.inner_rect.height(),
                clip: area.inner_rect,
            });
            data.insert_temp(bridge_id(), frame);
        }
    });
}

pub struct Reader {
    views: HashMap<MessageKey, Page>,
    failed: Vec<MessageKey>,
    spare: Option<Page>,
    context: Option<WebContext>,
    x11: Rc<Connection>,
    input: Rc<Input>,
    scroll_area: Option<ScrollArea>,
}

#[derive(Default)]
struct Input {
    wheel: Cell<f32>,
    links: RefCell<Vec<Url>>,
    keys: RefCell<Vec<egui::Event>>,
}

impl Reader {
    pub fn new(window: &impl HasWindowHandle) -> Result<Self> {
        let RawWindowHandle::Xlib(handle) = window.window_handle()?.as_raw() else {
            anyhow::bail!("The HTML pane requires an X11 window");
        };
        gdk::set_allowed_backends("x11");
        gtk::init()?;
        let display = gdk::Display::default()
            .context("GTK could not open the display")?
            .downcast::<gdkx11::X11Display>()
            .map_err(|_| anyhow::anyhow!("GTK could not open the X11 display"))?;
        let display = unsafe { gdkx11::ffi::gdk_x11_display_get_xdisplay(display.as_ptr()) };
        ensure!(!display.is_null(), "The X11 display is unavailable");
        let lib = xlib::Xlib::open()?;
        let display: *mut xlib::Display = display.cast();
        let mut class = xlib::XClassHint {
            res_name: c"tandem-mail".as_ptr().cast_mut(),
            res_class: c"io.tandem_mail.TandemMail".as_ptr().cast_mut(),
        };
        unsafe {
            (lib.XSetClassHint)(display, handle.window, &mut class);
        }
        Ok(Self {
            views: HashMap::new(),
            failed: Vec::new(),
            spare: None,
            context: None,
            x11: Rc::new(Connection {
                lib,
                display,
                parent: handle.window,
            }),
            input: Rc::new(Input::default()),
            scroll_area: None,
        })
    }

    pub fn begin_frame(&mut self, ctx: &Context) -> Vec<Url> {
        // GTK and the winit window share the UI thread.
        let main = glib::MainContext::default();
        for _ in 0..64 {
            if !main.iteration(false) {
                break;
            }
        }
        let delta = self.input.wheel.replace(0.);
        if delta != 0.
            && let Some(area) = self.scroll_area
        {
            let mut state = area.state;
            state.offset.y = (state.offset.y + delta).clamp(0., area.max_offset.max(0.));
            state.store(ctx, area.id);
        }
        ctx.input_mut(|input| input.events.append(&mut self.input.keys.borrow_mut()));
        let layouts = self
            .views
            .iter()
            .map(|(id, page)| (id.clone(), page.layout.get()))
            .chain(self.failed.drain(..).map(|id| (id, Layout::Failed)))
            .collect();
        ctx.data_mut(|data| {
            data.insert_temp(
                bridge_id(),
                FrameState {
                    layouts,
                    ..Default::default()
                },
            )
        });
        self.input.links.take()
    }

    pub fn end_frame(&mut self, ctx: &Context, covered: bool) {
        let Some(frame) = ctx.data_mut(|data| {
            let frame = data.get_temp::<FrameState>(bridge_id());
            data.remove::<FrameState>(bridge_id());
            frame
        }) else {
            return;
        };
        self.scroll_area = frame.scroll;
        // Keep one WebKit view warm across thread changes. A thread can still
        // have several independently expanded messages.
        self.views
            .extract_if(|id, _| !frame.bodies.iter().any(|body| &body.id == id))
            .for_each(|(_, mut page)| {
                if self.spare.is_none() && matches!(page.layout.get(), Layout::Ready(_)) {
                    page.hide();
                    self.spare = Some(page);
                }
            });
        let scale = ctx.pixels_per_point();
        let mut active = false;
        for body in frame.bodies {
            let mut visible = body.rect.intersect(body.clip);
            if let Some(area) = frame.scroll {
                visible = visible.intersect(area.clip);
            }
            if covered || !visible.is_positive() {
                if let Some(page) = self.views.get_mut(&body.id) {
                    page.hide();
                }
                continue;
            }
            let page = match self.views.entry(body.id.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let context = self.context.get_or_insert_with(WebContext::new_ephemeral);
                    let page = match self.spare.take() {
                        Some(mut page) => {
                            page.reset();
                            Ok(page)
                        }
                        None => Page::new(&self.x11, context, &self.input, ctx),
                    };
                    match page {
                        Ok(page) => entry.insert(page),
                        Err(error) => {
                            eprintln!("HTML reader: {error:#}");
                            self.failed.push(body.id);
                            ctx.request_repaint();
                            continue;
                        }
                    }
                }
            };
            page.show(&body, visible, scale);
            active = true;
        }
        if active {
            ctx.request_repaint_after(Duration::from_millis(32));
        }
    }
}

struct Connection {
    lib: xlib::Xlib,
    display: *mut xlib::Display,
    parent: c_ulong,
}

// The outer X11 window clips the webview to the egui scroll area.
struct ClipWindow {
    connection: Rc<Connection>,
    id: c_ulong,
}

impl ClipWindow {
    fn new(connection: &Rc<Connection>) -> Self {
        let id = unsafe {
            (connection.lib.XCreateSimpleWindow)(
                connection.display,
                connection.parent,
                0,
                0,
                1,
                1,
                0,
                0,
                0,
            )
        };
        Self {
            connection: connection.clone(),
            id,
        }
    }

    fn move_resize(&self, [left, top, right, bottom]: [i32; 4]) {
        let connection = &self.connection;
        unsafe {
            (connection.lib.XMoveResizeWindow)(
                connection.display,
                self.id,
                left,
                top,
                (right - left) as u32,
                (bottom - top) as u32,
            );
        }
    }

    fn set_visible(&self, visible: bool) {
        let connection = &self.connection;
        unsafe {
            if visible {
                (connection.lib.XMapWindow)(connection.display, self.id);
            } else {
                (connection.lib.XUnmapWindow)(connection.display, self.id);
            }
            (connection.lib.XFlush)(connection.display);
        }
    }
}

impl Drop for ClipWindow {
    fn drop(&mut self) {
        unsafe {
            (self.connection.lib.XDestroyWindow)(self.connection.display, self.id);
        }
    }
}

struct Page {
    window: gtk::Window,
    surface: gdk::Window,
    view: WebView,
    clip: ClipWindow,
    layout: Rc<Cell<Layout>>,
    generation: Rc<Cell<u32>>,
    scroll_top: Rc<Cell<f32>>,
    loading_since: Option<Instant>,
    document: Option<Arc<Document>>,
    colors: Option<(bool, bool)>,
    bounds: Option<([i32; 4], [i32; 4])>,
    visible: bool,
    scale: f32,
}

impl Page {
    fn new(
        connection: &Rc<Connection>,
        context: &WebContext,
        input: &Rc<Input>,
        ctx: &Context,
    ) -> Result<Self> {
        let clip = ClipWindow::new(connection);
        let window = gtk::Window::new(gtk::WindowType::Popup);
        let view = WebView::with_context(context);
        window.add(&view);
        window.realize();
        let surface = window
            .window()
            .context("WebKit window could not be realized")?;
        let xid = surface
            .downcast_ref::<gdkx11::X11Window>()
            .context("WebKit window is not on X11")?
            .xid();
        // Reparent before mapping so GTK's popup occupies the clipped part of
        // Tandem's window and never appears as a separate desktop window.
        unsafe {
            (connection.lib.XReparentWindow)(connection.display, xid, clip.id, 0, 0);
        }
        let settings = WebViewExt::settings(&view).context("WebKit settings unavailable")?;
        settings.set_enable_javascript_markup(false);
        settings.set_enable_dns_prefetching(false);
        settings.set_enable_developer_extras(false);
        settings.set_enable_webgl(false);
        settings.set_enable_webaudio(false);
        settings.set_enable_html5_local_storage(false);
        settings.set_allow_modal_dialogs(false);
        settings.set_javascript_can_open_windows_automatically(false);

        connect_input(&view, input, connection);
        let layout = Rc::new(Cell::new(Layout::Loading));
        let generation = Rc::new(Cell::new(0));
        let scroll_top = Rc::new(Cell::new(0.));
        connect_layout(&view, &layout, &generation, &scroll_top, ctx)?;
        window.show_all();
        Ok(Self {
            window,
            surface,
            view,
            clip,
            layout,
            generation,
            scroll_top,
            loading_since: None,
            document: None,
            colors: None,
            bounds: None,
            visible: false,
            scale: 0.,
        })
    }

    fn show(&mut self, body: &Body, visible: Rect, scale: f32) {
        let edges = |rect: Rect| {
            [rect.left(), rect.top(), rect.right(), rect.bottom()]
                .map(|point| (point * scale).round() as i32)
        };
        let clip = edges(visible);
        if clip[0] >= clip[2] || clip[1] >= clip[3] {
            self.hide();
            return;
        }
        // Allocate at most two reading panes of native content. The outer
        // scroll area can still represent the full document on HiDPI displays.
        let page_height = body.clip.height().max(480.);
        let view_height = body.rect.height().min(page_height * 2.);
        let visible_offset = visible.top() - body.rect.top();
        let section = (visible_offset / page_height).floor() * page_height;
        let scroll_top = section.min(body.rect.height() - view_height).max(0.);
        let mut viewport = body.rect;
        viewport.min.y += scroll_top;
        viewport.max.y = viewport.min.y + view_height;
        let rect = edges(viewport);
        if !self.window.is_visible() {
            self.window.show_all();
        }
        if self.bounds != Some((rect, clip)) {
            self.clip.move_resize(clip);
            let gdk_scale = self.surface.scale_factor();
            let logical = |pixels: i32| (pixels as f32 / gdk_scale as f32).round() as i32;
            let width = logical(rect[2] - rect[0]).max(1);
            let height = logical(rect[3] - rect[1]).max(1);
            self.surface.move_resize(
                logical(rect[0] - clip[0]),
                logical(rect[1] - clip[1]),
                width,
                height,
            );
            self.window
                .size_allocate(&gtk::Allocation::new(0, 0, width, height));
            self.bounds = Some((rect, clip));
        }
        if self.scale != scale {
            self.scale = scale;
            self.view
                .set_zoom_level(f64::from(scale) / f64::from(self.view.scale_factor()));
        }
        if self.colors != Some((body.theme.dark, body.original_colors)) {
            apply_colors(&self.view, body.theme, body.original_colors);
            self.colors = Some((body.theme.dark, body.original_colors));
        }
        let scroll_changed = self.scroll_top.replace(scroll_top) != scroll_top;
        if self
            .document
            .as_ref()
            .is_none_or(|document| !Arc::ptr_eq(document, &body.document))
        {
            self.generation.set(self.generation.get().wrapping_add(1));
            let html = body.document.html.replacen(
                "<!doctype html>",
                &format!(
                    r#"<!doctype html><meta name="tandem-layout" content="{}">"#,
                    self.generation.get()
                ),
                1,
            );
            self.view.load_html(&html, None);
            self.document = Some(body.document.clone());
            self.loading_since = Some(Instant::now());
        } else if scroll_changed && !self.view.is_loading() {
            self.view.evaluate_javascript(
                &format!("window.tandemScrollTop = {scroll_top}; window.scrollTo(window.scrollX, {scroll_top});"),
                Some(WORLD), None, gtk::gio::Cancellable::NONE, |_| {},
            );
        }
        if matches!(self.layout.get(), Layout::Loading)
            && self
                .loading_since
                .is_some_and(|since| since.elapsed() >= Duration::from_secs(10))
        {
            self.layout.set(Layout::Failed);
        }
        if !self.visible && matches!(self.layout.get(), Layout::Ready(_)) {
            self.clip.set_visible(true);
            self.visible = true;
        }
    }

    fn reset(&mut self) {
        self.layout.set(Layout::Loading);
        self.document = None;
    }

    fn hide(&mut self) {
        if self.visible {
            self.clip.set_visible(false);
            self.visible = false;
        }
        self.window.hide();
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        // The GTK-owned child must go away before the X11 clipping window.
        unsafe {
            self.window.destroy();
        }
    }
}

fn connect_input(view: &WebView, input: &Rc<Input>, connection: &Rc<Connection>) {
    let links = input.clone();
    view.connect_decide_policy(move |_, decision, kind| {
        if kind == PolicyDecisionType::Response {
            return false;
        }
        if let Some(policy) = decision.downcast_ref::<NavigationPolicyDecision>()
            && let Some(action) = policy.navigation_action()
            && let Some(uri) = action.request().and_then(|request| request.uri())
        {
            if kind == PolicyDecisionType::NavigationAction && uri == "about:blank" {
                decision.use_();
                return true;
            }
            if action.navigation_type() == NavigationType::LinkClicked
                && action.is_user_gesture()
                && let Some(url) = web_url(&uri)
            {
                links.links.borrow_mut().push(url);
            }
        }
        decision.ignore();
        true
    });
    view.connect_context_menu(|_, menu, _, _| {
        for item in menu.items() {
            if !matches!(
                item.stock_action(),
                ContextMenuAction::Copy
                    | ContextMenuAction::CopyLinkToClipboard
                    | ContextMenuAction::SelectAll
            ) {
                menu.remove(&item);
            }
        }
        false
    });

    let wheel = input.clone();
    view.connect_scroll_event(move |_, event| {
        let delta = match event.direction() {
            gdk::ScrollDirection::Up => -48.,
            gdk::ScrollDirection::Down => 48.,
            gdk::ScrollDirection::Smooth => (event.delta().1 * 48.) as f32,
            _ => return glib::Propagation::Proceed,
        };
        wheel.wheel.set(wheel.wheel.get() + delta);
        glib::Propagation::Stop
    });

    let x11 = connection.clone();
    view.connect_button_press_event(move |view, event| {
        if let Some(window) = view
            .toplevel()
            .and_then(|widget| widget.window())
            .and_then(|window| window.downcast::<gdkx11::X11Window>().ok())
        {
            unsafe {
                (x11.lib.XSetInputFocus)(
                    x11.display,
                    window.xid(),
                    xlib::RevertToParent,
                    event.time().into(),
                );
            }
            view.grab_focus();
        }
        glib::Propagation::Proceed
    });
    let x11 = connection.clone();
    let input = input.clone();
    view.connect_key_press_event(move |_, event| {
        let state = event.state();
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let alt = state.contains(gdk::ModifierType::MOD1_MASK);
        let key = event.keyval().name();
        if ctrl
            && key
                .as_deref()
                .is_some_and(|key| matches!(key.to_ascii_lowercase().as_str(), "a" | "c"))
        {
            return glib::Propagation::Proceed;
        }
        if !ctrl && !alt {
            let delta = match key.as_deref() {
                Some("Down") => 48.,
                Some("Up") => -48.,
                Some("Page_Down" | "space") if !shift => 520.,
                Some("Page_Up" | "space") => -520.,
                _ => 0.,
            };
            if delta != 0. {
                input.wheel.set(input.wheel.get() + delta);
                return glib::Propagation::Stop;
            }
        }
        let Some(key) = key.as_deref().and_then(|key| egui::Key::from_name(key)) else {
            return glib::Propagation::Proceed;
        };
        input.keys.borrow_mut().push(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                alt,
                ctrl,
                shift,
                command: ctrl,
                mac_cmd: false,
            },
        });
        // Return keyboard focus to Tandem so a shortcut can open an editor.
        unsafe {
            (x11.lib.XSetInputFocus)(
                x11.display,
                x11.parent,
                xlib::RevertToParent,
                event.time().into(),
            );
        }
        glib::Propagation::Stop
    });
}

fn connect_layout(
    view: &WebView,
    layout: &Rc<Cell<Layout>>,
    generation: &Rc<Cell<u32>>,
    scroll_top: &Rc<Cell<f32>>,
    ctx: &Context,
) -> Result<()> {
    let manager = view
        .user_content_manager()
        .context("WebKit content manager unavailable")?;
    let layout_changed = layout.clone();
    let repaint = ctx.clone();
    let document_generation = generation.clone();
    manager.connect_script_message_received(Some("height"), move |_, result| {
        if let Some(value) = result.js_value()
            && let Some((generation, height)) = value.to_str().split_once(':')
            && generation.parse::<u32>() == Ok(document_generation.get())
            && let Ok(height) = height.parse::<f32>()
            && height.is_finite()
            && height > 0.
        {
            let height = height.clamp(16., 1_000_000.);
            if !matches!(layout_changed.get(), Layout::Ready(old) if old == height) {
                layout_changed.set(Layout::Ready(height));
                repaint.request_repaint();
            }
        }
    });
    ensure!(
        manager.register_script_message_handler_in_world("height", WORLD),
        "Could not set up HTML layout reporting"
    );
    let layout_loaded = layout.clone();
    let repaint = ctx.clone();
    let document_generation = generation.clone();
    let scroll_top = scroll_top.clone();
    view.connect_load_changed(move |view, event| {
        if event == LoadEvent::Finished {
            let layout = layout_loaded.clone();
            let repaint = repaint.clone();
            let expected_generation = document_generation.get();
            let generation = document_generation.clone();
            view.evaluate_javascript(
                &format!("window.tandemScrollTop = {}; {}", scroll_top.get(), r#"(() => {
                    const generation = document.querySelector('meta[name="tandem-layout"]')?.content;
                    if (!generation) return;
                    const report = () => {
                        const height = Math.max(document.body.offsetHeight, document.body.scrollHeight);
                        window.webkit.messageHandlers.height.postMessage(generation + ":" + Math.ceil(height + 16));
                    };
                    new ResizeObserver(report).observe(document.body);
                    const reflow = () => {
                        window.scrollTo(window.scrollX, window.tandemScrollTop);
                        report();
                    };
                    window.addEventListener("resize", reflow);
                    reflow();
                })();"#),
                Some(WORLD), None, gtk::gio::Cancellable::NONE,
                move |result| {
                    if let Err(error) = result && generation.get() == expected_generation {
                        eprintln!("HTML reader: {error}");
                        layout.set(Layout::Failed);
                        repaint.request_repaint();
                    }
                },
            );
        }
    });
    let layout_failed = layout.clone();
    let repaint = ctx.clone();
    view.connect_web_process_terminated(move |_, reason| {
        eprintln!("HTML reader: WebKit terminated ({reason:?})");
        layout_failed.set(Layout::Failed);
        repaint.request_repaint();
    });
    Ok(())
}

fn apply_colors(view: &WebView, theme: Theme, original: bool) {
    let css_color =
        |color: Color32| format!("#{:02x}{:02x}{:02x}", color.r(), color.g(), color.b());
    let mut css = String::from(
        "html { overflow-y: hidden !important; scroll-behavior: auto !important; }
         body { display: flow-root !important; }",
    );
    if original {
        css.push_str("html { color-scheme: light !important; }");
    } else {
        let scheme = if theme.dark { "dark" } else { "light" };
        let (background, text, link, border) = (
            css_color(theme.surface),
            css_color(theme.ink),
            css_color(theme.accent),
            css_color(theme.line),
        );
        css.push_str(&format!(
            "html {{ color-scheme: {scheme} !important; background: {background} !important; }}
             html * {{
                 color: {text} !important;
                 background-color: transparent !important;
                 border-color: {border} !important;
                 text-shadow: none !important;
                 box-shadow: none !important;
             }}
             html a, html a * {{ color: {link} !important; }}
             html :not(img) {{ background-image: none !important; }}"
        ));
    }
    if let Some(manager) = view.user_content_manager() {
        manager.remove_all_style_sheets();
        manager.add_style_sheet(&UserStyleSheet::new(
            &css,
            UserContentInjectedFrames::TopFrame,
            UserStyleLevel::User,
            &[],
            &[],
        ));
    }
    let background = if original {
        Color32::WHITE
    } else {
        theme.surface
    };
    view.set_background_color(&gdk::RGBA::new(
        f64::from(background.r()) / 255.,
        f64::from(background.g()) / 255.,
        f64::from(background.b()) / 255.,
        1.,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a graphical X11 session with WebKitGTK"]
    fn long_html_message_scrolls_at_double_scale() -> Result<()> {
        struct Parent(c_ulong);
        impl HasWindowHandle for Parent {
            fn window_handle(
                &self,
            ) -> std::result::Result<
                raw_window_handle::WindowHandle<'_>,
                raw_window_handle::HandleError,
            > {
                let raw = raw_window_handle::XlibWindowHandle::new(self.0);
                Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw.into()) })
            }
        }

        fn wait_until(mut condition: impl FnMut() -> bool) -> Result<()> {
            let main = glib::MainContext::default();
            let deadline = Instant::now() + Duration::from_secs(8);
            while !condition() {
                ensure!(Instant::now() < deadline, "WebKit did not finish");
                if !main.iteration(false) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(())
        }

        gdk::set_allowed_backends("x11");
        gtk::init()?;
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_default_size(1000, 700);
        window.show_all();
        let xid = window
            .window()
            .context("Missing X11 window")?
            .downcast::<gdkx11::X11Window>()
            .map_err(|_| anyhow::anyhow!("Expected X11"))?
            .xid();
        let reader = Reader::new(&Parent(xid))?;
        let ctx = Context::default();
        let context = WebContext::new_ephemeral();
        let mut view = Page::new(&reader.x11, &context, &reader.input, &ctx)?;

        let raw = format!(
            "Content-Type: text/html\r\n\r\n<body style='margin:0;background:#fff'>{}<p>End of newsletter</p></body>",
            "<p style='height:100px;margin:0'>Newsletter section</p>".repeat(100),
        );
        let document = crate::html_mail::from_mime(raw.as_bytes(), |_| None).unwrap();
        let clip = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(450., 300.));
        let mut body = Body {
            id: ("demo".into(), "long".into()),
            document: Arc::new(document),
            theme: Theme::new(true),
            original_colors: false,
            rect: clip,
            clip,
        };
        view.show(&body, clip, 2.);
        wait_until(|| matches!(view.layout.get(), Layout::Ready(_)))?;
        let Layout::Ready(height) = view.layout.get() else {
            unreachable!()
        };
        assert!(height > 10_000.);
        body.rect.min.y = 300. - height;
        view.show(&body, clip, 2.);

        let measured = Rc::new(RefCell::new(None));
        let result = measured.clone();
        view.view.evaluate_javascript(
            r#"JSON.stringify({
                scroll: scrollY,
                viewport: innerHeight,
                bottom: document.querySelector("p:last-child").getBoundingClientRect().bottom,
                background: getComputedStyle(document.documentElement).backgroundColor
            })"#,
            Some(WORLD),
            None,
            gtk::gio::Cancellable::NONE,
            move |value| *result.borrow_mut() = Some(value),
        );
        wait_until(|| measured.borrow().is_some())?;
        let result = measured.take().unwrap()?.to_str();
        let result: serde_json::Value = serde_json::from_str(&result)?;
        assert!(result["scroll"].as_f64().unwrap() > 8_200., "{result}");
        assert!(result["viewport"].as_f64().unwrap() <= 960., "{result}");
        assert_eq!(result["background"], "rgb(40, 40, 40)");
        Ok(())
    }
}
