use std::{
    collections::HashMap,
    io::Cursor,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, ensure};
use eframe::egui::{
    self, ColorImage, CursorIcon, FontData, FontDefinitions, FontFamily, FontId, Frame, Id,
    ImageData, Label, Margin, Sense, Stroke, TextureHandle, TextureOptions, Ui,
    text::{Galley, LayoutJob, TextFormat},
};
use futures::{StreamExt, stream};
use image::{AnimationDecoder, ImageDecoder, metadata::LoopCount};
use reqwest::Url;
use tokio::sync::{Semaphore, oneshot};

use super::{MailApp, Theme};
use crate::{
    html_mail,
    message_body::{self, Block, Body, HtmlLayout, ImageSource, Run, TableCell},
    model::{Message, ThreadKey},
    remote_image,
    store::{BodyView, Store},
};

mod cache;
pub(super) use cache::{ReaderContent, RenderSize};

struct ReaderMessage {
    body: Body,
    view: BodyView,
    images: HashMap<usize, Asset>,
    raw: Option<Arc<[u8]>>,
    html: HtmlReader,
}

impl ReaderMessage {
    fn uses_original_html(&self) -> bool {
        match self.view {
            BodyView::Auto => self.body.layout == Some(HtmlLayout::Designed),
            BodyView::Original => self.body.layout.is_some(),
            BodyView::Selectable => false,
        }
    }
}

#[derive(Default)]
struct HtmlReader {
    page: Option<PaintedPage>,
    pending: Option<HtmlJob>,
    width: u32,
    density: u32,
    stopped: bool,
    attempted_full_load: bool,
    retry_images: bool,
}

type HtmlJob = oneshot::Receiver<Result<Option<PaintedPage>, String>>;

struct PaintedPage {
    size: egui::Vec2,
    density: f32,
    tiles: Vec<TextureHandle>,
    links: Vec<html_mail::Link>,
    gifs: Vec<(html_mail::Bounds, AnimatedImage)>,
    missing_images: bool,
}

impl HtmlReader {
    fn show(
        &mut self,
        ui: &mut Ui,
        runtime: &tokio::runtime::Runtime,
        cache: &Path,
        raw: &Arc<[u8]>,
        message: &str,
    ) -> Option<Option<Url>> {
        if let Some(pending) = &mut self.pending {
            match pending.try_recv() {
                Ok(Ok(Some(page))) => {
                    self.page = Some(page);
                    self.pending = None;
                }
                Ok(Ok(None)) => {
                    self.stopped = true;
                    self.pending = None;
                }
                Ok(Err(error)) => {
                    eprintln!("HTML reader: {error}");
                    self.stopped = true;
                    self.pending = None;
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.stopped = true;
                    self.pending = None;
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
        }

        let RenderSize { width, density } = RenderSize::new(ui.ctx(), ui.available_width());
        let load_images = self.retry_images
            || (!self.attempted_full_load
                && self.page.as_ref().is_some_and(|page| page.missing_images));
        if !self.stopped
            && self.pending.is_none()
            && (self.width.abs_diff(width) >= 16 || self.density != density || load_images)
        {
            let (mut send, recv) = oneshot::channel();
            let (ctx, cache, raw) = (ui.ctx().clone(), cache.to_owned(), raw.clone());
            let message = message.to_owned();
            self.retry_images = false;
            self.attempted_full_load |= load_images;
            self.pending = Some(recv);
            self.width = width;
            self.density = density;
            runtime.spawn(async move {
                let result = tokio::select! {
                    _ = send.closed() => return,
                    result = PaintedPage::prepare(&ctx, cache, raw, RenderSize { width, density }, load_images, &message) => result,
                };
                let _ = send.send(result.map_err(|err| format!("{err:#}")));
                ctx.request_repaint();
            });
        }

        let page = self.page.as_mut()?;
        let clicked = page.draw(ui, message);
        if page.missing_images
            && self.pending.is_none()
            && self.attempted_full_load
            && !self.stopped
            && ui.small_button("Retry missing images").clicked()
        {
            self.retry_images = true;
            ui.ctx().request_repaint();
        }
        Some(clicked)
    }
}

impl PaintedPage {
    async fn prepare(
        ctx: &egui::Context,
        cache: PathBuf,
        raw: Arc<[u8]>,
        size: RenderSize,
        load_images: bool,
        message: &str,
    ) -> Result<Option<Self>> {
        let Some(page) = prepare_html(cache, raw, size.width, size.density, load_images).await?
        else {
            return Ok(None);
        };
        let tiles = page
            .tiles
            .into_iter()
            .enumerate()
            .map(|(index, tile)| {
                ctx.load_texture(
                    format!("html-mail:{message}:{index}"),
                    ImageData::Color(color_image(tile)),
                    TextureOptions::LINEAR,
                )
            })
            .collect();
        let gifs = stream::iter(page.animated.into_iter().enumerate())
            .map(|(index, (bounds, bytes))| async move {
                let pixels = decode_image(bytes).await.ok()?;
                let image = AnimatedImage::new(ctx, format!("html-gif:{message}:{index}"), pixels);
                Some((bounds, image))
            })
            .buffered(2)
            .filter_map(std::future::ready)
            .collect()
            .await;
        Ok(Some(Self {
            size: egui::vec2(page.width as f32, page.height as f32),
            density: page.density as f32,
            tiles,
            links: page.links,
            gifs,
            missing_images: !page.missing.is_empty(),
        }))
    }

    fn draw(&mut self, ui: &mut Ui, message: &str) -> Option<Url> {
        let scale = (ui.available_width() / self.size.x).min(1.);
        let (rect, _) = ui.allocate_exact_size(self.size * scale, Sense::hover());
        let painter = ui.painter();
        let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1., 1.));
        let mut top = rect.top();
        for tile in &self.tiles {
            let tile_rect = egui::Rect::from_min_size(
                egui::pos2(rect.left(), top),
                tile.size_vec2() * (scale / self.density),
            );
            if ui.is_rect_visible(tile_rect) {
                painter.image(tile.id(), tile_rect, uv, egui::Color32::WHITE);
            }
            top = tile_rect.bottom();
        }

        let place = |bounds: html_mail::Bounds| {
            egui::Rect::from_min_size(
                rect.min + egui::vec2(bounds.x, bounds.y) * scale,
                egui::vec2(bounds.width, bounds.height) * scale,
            )
        };
        for (bounds, image) in &mut self.gifs {
            let image_rect = place(*bounds);
            if ui.is_rect_visible(image_rect) {
                painter.with_clip_rect(rect).image(
                    image.texture(ui.ctx()).id(),
                    image_rect,
                    uv,
                    egui::Color32::WHITE,
                );
            }
        }

        self.links.iter().enumerate().find_map(|(index, link)| {
            let hit = place(link.bounds).intersect(rect);
            if !ui.is_rect_visible(hit) || !hit.is_positive() {
                return None;
            }
            let url = message_body::web_url(&link.url)?;
            ui.interact(
                hit,
                ui.id().with(("html-link", message, index)),
                Sense::click(),
            )
            .on_hover_cursor(CursorIcon::PointingHand)
            .on_hover_text(url.as_str())
            .clicked()
            .then_some(url)
        })
    }
}

async fn prepare_html(
    cache: PathBuf,
    raw: Arc<[u8]>,
    width: u32,
    density: u32,
    load_images: bool,
) -> Result<Option<html_mail::Page>> {
    let store = Store::open_read_only(&cache)?;
    let Some(mut document) = cached_document(&store, &raw) else {
        return Ok(None);
    };
    let urls: Vec<_> = document
        .missing
        .iter()
        .take(if load_images { usize::MAX } else { 24 })
        .cloned()
        .collect();
    if !urls.is_empty() {
        let mut downloads = stream::iter(urls)
            .map(|url| {
                let cache = &cache;
                async move { remote_image::load(cache, &url).await.is_ok() }
            })
            .buffer_unordered(4);
        let budget = if load_images {
            Duration::from_secs(10)
        } else {
            Duration::from_millis(1200)
        };
        let deadline = tokio::time::Instant::now() + budget;
        let mut changed = false;
        while let Ok(Some(result)) = tokio::time::timeout_at(deadline, downloads.next()).await {
            changed |= result;
        }
        if changed {
            document = cached_document(&store, &raw).context("Missing HTML body")?;
        }
    }
    html_mail::render_cached(
        document,
        width,
        density,
        &cache.with_file_name("render-cache.sqlite"),
    )
    .await
    .map(Some)
}

fn cached_document(store: &Store, raw: &[u8]) -> Option<html_mail::Document> {
    html_mail::from_mime(raw, |url| store.remote_image(url.as_str()).ok().flatten())
}

enum Asset {
    Loading(oneshot::Receiver<Result<DecodedImage, String>>),
    Ready(AnimatedImage),
    Failed(String),
}

impl Asset {
    fn poll(&mut self, ctx: &egui::Context, id: impl FnOnce() -> String) {
        if let Asset::Loading(receiver) = self {
            match receiver.try_recv() {
                Ok(Ok(pixels)) => *self = Asset::Ready(AnimatedImage::new(ctx, id(), pixels)),
                Ok(Err(error)) => *self = Asset::Failed(error),
                Err(oneshot::error::TryRecvError::Closed) => {
                    *self = Asset::Failed("The image could not be loaded".into());
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
        }
    }
}

struct DecodedImage {
    frames: Vec<(Arc<ColorImage>, Duration)>,
    loops: Option<u32>,
}

impl DecodedImage {
    fn bytes(&self) -> usize {
        self.frames
            .iter()
            .map(|(image, _)| image.pixels.len() * 4)
            .sum()
    }
}

struct AnimatedImage {
    texture: TextureHandle,
    decoded: DecodedImage,
    started: Option<Instant>,
    frame: usize,
}

impl AnimatedImage {
    fn new(ctx: &egui::Context, name: String, decoded: DecodedImage) -> Self {
        let texture = ctx.load_texture(
            name,
            ImageData::Color(decoded.frames[0].0.clone()),
            TextureOptions::LINEAR,
        );
        Self {
            texture,
            decoded,
            started: None,
            frame: 0,
        }
    }

    fn texture(&mut self, ctx: &egui::Context) -> &TextureHandle {
        let frames = &self.decoded.frames;
        if frames.len() > 1 {
            let elapsed = self.started.get_or_insert_with(Instant::now).elapsed();
            let total = frames.last().unwrap().1;
            let complete = self
                .decoded
                .loops
                .is_some_and(|loops| elapsed >= total.saturating_mul(loops));
            let next = if complete {
                frames.len() - 1
            } else {
                let phase = Duration::from_millis((elapsed.as_millis() % total.as_millis()) as u64);
                let next = frames.partition_point(|(_, end)| *end <= phase);
                ctx.request_repaint_after(frames[next].1 - phase);
                next
            };
            if next != self.frame {
                self.texture.set(
                    ImageData::Color(frames[next].0.clone()),
                    TextureOptions::LINEAR,
                );
                self.frame = next;
            }
        }
        &self.texture
    }
}

pub(super) fn load_font(ctx: &egui::Context) {
    if let Some(path) = std::env::var_os("TANDEM_MAIL_BOLD_FONT")
        && let Ok(bytes) = std::fs::read(path)
    {
        let mut fonts = FontDefinitions::default();
        let name = "tandem-bold".to_owned();
        fonts
            .font_data
            .insert(name.clone(), Arc::new(FontData::from_owned(bytes)));
        let mut family = fonts.families[&FontFamily::Proportional].clone();
        family.insert(0, name.clone());
        let bold = FontFamily::Name(name.into());
        fonts.families.insert(bold.clone(), family);
        ctx.set_fonts(fonts);
        ctx.data_mut(|data| data.insert_temp(Id::new("tandem-bold-font"), bold));
    }
}

pub(super) fn bold_family(ctx: &egui::Context) -> Option<FontFamily> {
    let family = ctx.data(|data| data.get_temp(Id::new("tandem-bold-font")))?;
    ctx.fonts(|fonts| fonts.definitions().families.contains_key(&family))
        .then_some(family)
}

impl MailApp {
    pub(super) fn message_body(&mut self, ui: &mut Ui, key: &ThreadKey, message: &Message) {
        let reader_key = (key.account.clone(), message.id.clone());
        let cached = if self.demo {
            self.reader_content
                .open_demo(&self.store, &reader_key, &message.body)
        } else {
            self.reader_content
                .open(ui, &self.runtime, &self.path, &reader_key)
        };
        let Some(cached) = cached else {
            show_body_preview(ui, &message.body);
            return;
        };
        let theme = self.theme;
        if !self.demo
            && cached.uses_original_html()
            && let Some(raw) = &cached.raw
            && let Some(clicked) = cached
                .html
                .show(ui, &self.runtime, &self.path, raw, &message.id)
        {
            if let Some(url) = clicked {
                self.open_mail_link(&url);
            }
            return;
        }
        if cached.uses_original_html() && cached.html.pending.is_some() {
            show_body_preview(ui, &message.body);
            return;
        }
        let mut clicked = None;
        ui.scope(|ui| {
            ui.set_max_width(ui.available_width().min(750.));
            for (index, block) in cached.body.blocks.iter().enumerate() {
                match block {
                    Block::Text { indent, marker, runs } => {
                        indent_block(ui, *indent, |ui| {
                            let link = if let Some(marker) = marker {
                                ui.horizontal_top(|ui| {
                                    ui.label(theme.muted(marker));
                                    inline_text(ui, runs, theme)
                                }).inner
                            } else if runs.iter().all(|run| run.style.centered) {
                                ui.vertical_centered(|ui| inline_text(ui, runs, theme)).inner
                            } else {
                                inline_text(ui, runs, theme)
                            };
                            if let Some(url) = link {
                                clicked = Some(url);
                            }
                        });
                    }
                    Block::TableRow { indent, cells } => {
                        indent_block(ui, *indent, |ui| {
                            if let Some(url) = table_row(ui, cells, theme) {
                                clicked = Some(url);
                            }
                        });
                    }
                    Block::Image {
                        indent,
                        source,
                        alt,
                        width,
                        centered,
                        link,
                    } => {
                        if self.demo && matches!(source, ImageSource::Remote(_)) {
                            continue;
                        }
                        let state = cached.images.entry(index).or_insert_with(|| {
                            start_image(&self.runtime, ui.ctx(), self.path.clone(), source)
                        });
                        let mut draw = |ui: &mut Ui| {
                            state.poll(ui.ctx(), || format!("mail:{}:{index}", message.id));
                            match state {
                                Asset::Failed(error) => {
                                    let label = if alt == "Image" { "Retry image".to_owned() } else { format!("Retry image · {alt}") };
                                    if ui.small_button(label).on_hover_text(error.as_str()).clicked() {
                                        *state = start_image(
                                            &self.runtime,
                                            ui.ctx(),
                                            self.path.clone(),
                                            source,
                                        );
                                    }
                                }
                                Asset::Loading(_) => {
                                    ui.horizontal(|ui| {
                                        ui.spinner();
                                        ui.label(theme.muted(alt));
                                    });
                                }
                                Asset::Ready(image) => {
                                    let texture = image.texture(ui.ctx());
                                    let native = texture.size_vec2();
                                    let width =
                                        width.unwrap_or(native.x).min(ui.available_width()).max(1.);
                                    let size = native * (width / native.x);
                                    let image = egui::Image::new((texture.id(), size))
                                        .alt_text(alt)
                                        .sense(if link.is_some() {
                                            Sense::click()
                                        } else {
                                            Sense::hover()
                                        });
                                    let response = ui.add(image);
                                    if let Some(url) = link
                                        && response
                                            .on_hover_cursor(CursorIcon::PointingHand)
                                            .on_hover_text(url.as_str())
                                            .clicked()
                                    {
                                        clicked = Some(url.clone());
                                    }
                                }
                            }
                        };
                        indent_block(ui, *indent, |ui| {
                            if matches!(source, ImageSource::Remote(url) if message_body::ed_logo(url)) {
                                Frame::new()
                                    .fill(egui::Color32::from_rgb(80, 40, 140))
                                    .inner_margin(Margin::same(8))
                                    .corner_radius(6)
                                    .show(ui, |ui| { ui.vertical_centered(draw); });
                            } else if *centered {
                                ui.vertical_centered(draw);
                            } else {
                                draw(ui);
                            }
                        });
                    }
                    Block::Divider => {
                        ui.separator();
                    }
                }
            }
        });
        if let Some(url) = clicked {
            self.open_mail_link(&url);
        }
    }

    fn open_mail_link(&mut self, url: &Url) {
        let opened = ["brave", "brave-browser"].into_iter().any(|bin| {
            std::process::Command::new(bin)
                .arg(url.as_str())
                .spawn()
                .is_ok()
        });
        if !opened {
            self.toast("Could not open Brave", true);
        }
    }

    pub(super) fn body_view_options(&mut self, ui: &mut Ui, key: &ThreadKey, message: &str) {
        let Some(reader) = self
            .reader_content
            .get_mut(&(key.account.clone(), message.to_owned()))
        else {
            return;
        };
        if self.demo || reader.body.layout.is_none() {
            return;
        }
        let (caption, alternate) = if reader.uses_original_html() && !reader.html.stopped {
            ("Show selectable text", BodyView::Selectable)
        } else {
            ("Show original HTML", BodyView::Original)
        };
        let view = if ui.button(caption).clicked() {
            alternate
        } else if reader.view != BodyView::Auto && ui.button("Use automatic view").clicked() {
            BodyView::Auto
        } else {
            return;
        };
        match self.store.set_body_view(&key.account, message, view) {
            Ok(()) => {
                reader.view = view;
                if reader.html.pending.is_some() || reader.html.stopped {
                    reader.html = HtmlReader::default();
                }
                ui.close();
            }
            Err(error) => self.toast(format!("Could not save message view: {error}"), true),
        }
    }
}

fn show_body_preview(ui: &mut Ui, body: &str) {
    let end = body.floor_char_boundary(body.len().min(32 * 1024));
    ui.add(Label::new(&body[..end]).selectable(true).wrap());
}

fn indent_block(ui: &mut Ui, depth: usize, draw: impl FnOnce(&mut Ui)) {
    if depth == 0 {
        draw(ui);
    } else {
        let margin = (depth.min(7) * 14) as i8;
        Frame::new()
            .inner_margin(Margin {
                left: margin,
                ..Margin::ZERO
            })
            .show(ui, draw);
    }
}

fn table_row(ui: &mut Ui, cells: &[TableCell], theme: Theme) -> Option<Url> {
    let mut link = None;
    Frame::new()
        .fill(theme.raised)
        .inner_margin(Margin::same(8))
        .corner_radius(6)
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let spacing = ui.spacing().item_spacing.x * (cells.len() - 1) as f32;
                let total = (ui.available_width() - spacing).max(cells.len() as f32);
                let share = total / cells.len() as f32;
                let fixed_width = |width: f32| width.min(share);
                let fixed: f32 = cells
                    .iter()
                    .filter_map(|cell| cell.width.map(fixed_width))
                    .sum();
                let flexible = cells
                    .iter()
                    .filter(|cell| cell.width.is_none())
                    .count()
                    .max(1);
                for cell in cells {
                    let width = cell
                        .width
                        .map_or((total - fixed) / flexible as f32, fixed_width);
                    let align = if cell.centered {
                        egui::Align::Center
                    } else {
                        egui::Align::Min
                    };
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, 0.),
                        egui::Layout::top_down(align),
                        |ui| {
                            ui.set_min_width(width);
                            ui.set_max_width(width);
                            for runs in &cell.paragraphs {
                                if let Some(url) = inline_text(ui, runs, theme) {
                                    link = Some(url);
                                }
                            }
                        },
                    );
                }
            });
        });
    link
}

fn inline_text(ui: &mut Ui, runs: &[Run], theme: Theme) -> Option<Url> {
    if let Some(url) = runs.first().and_then(|run| run.style.link.as_ref())
        && runs
            .iter()
            .all(|run| run.style.button && run.style.link.as_ref() == Some(url))
    {
        let caption: String = runs.iter().map(|run| run.text.as_str()).collect();
        return theme
            .primary(ui, &caption)
            .on_hover_text(url.as_str())
            .clicked()
            .then(|| url.clone());
    }
    let mut job = LayoutJob::default();
    job.wrap.max_width = ui.available_width();
    let mut links: Vec<(Range<usize>, &Url)> = Vec::new();
    let mut cursor = 0;
    let bold_font = bold_family(ui.ctx());
    for run in runs {
        let style = &run.style;
        let color = if style.link.is_some() {
            theme.accent
        } else {
            theme.ink
        };
        let family = if style.mono {
            FontFamily::Monospace
        } else if let Some(bold) = &bold_font
            && style.bold
        {
            bold.clone()
        } else {
            FontFamily::Proportional
        };
        let size = if let Some(size) = style.font_size {
            size
        } else if style.heading > 0 {
            24. - f32::from(style.heading) * 1.5
        } else if style.bold && bold_font.is_none() {
            14.5
        } else {
            14.
        };
        job.append(
            &run.text,
            0.,
            TextFormat {
                font_id: FontId::new(size, family),
                line_height: style.line_height.map(|scale| size * scale),
                color,
                italics: style.italic,
                underline: if style.link.is_some() || style.underline {
                    Stroke::new(1., color)
                } else {
                    Stroke::NONE
                },
                strikethrough: if style.strike {
                    Stroke::new(1., color)
                } else {
                    Stroke::NONE
                },
                ..Default::default()
            },
        );
        let end = cursor + run.text.chars().count();
        if let Some(url) = &style.link {
            links.push((cursor..end, url));
        }
        cursor = end;
    }

    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let label = Label::new(galley.clone()).selectable(true);
    let response = ui.add(if links.is_empty() {
        label
    } else {
        label.sense(Sense::click())
    });
    let pos = response.hover_pos()?;
    let char_index = glyph_at(&galley, pos - response.rect.min)?;
    let (_, url) = links
        .into_iter()
        .find(|(range, _)| range.contains(&char_index))?;
    response
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(url.as_str())
        .clicked()
        .then(|| url.clone())
}

fn glyph_at(galley: &Galley, point: egui::Vec2) -> Option<usize> {
    let mut start = 0;
    for row in &galley.rows {
        if row.rect().contains(egui::pos2(point.x, point.y)) {
            let x = point.x - row.pos.x;
            return row
                .glyphs
                .iter()
                .position(|glyph| x >= glyph.pos.x && x < glyph.max_x())
                .map(|column| start + column);
        }
        start += row.char_count_including_newline();
    }
    None
}

fn start_image(
    runtime: &tokio::runtime::Runtime,
    ctx: &egui::Context,
    path: PathBuf,
    source: &ImageSource,
) -> Asset {
    let (mut send, recv) = oneshot::channel();
    let ctx = ctx.clone();
    let source = source.clone();
    runtime.spawn(async move {
        let result = tokio::select! {
            _ = send.closed() => return,
            result = async move {
                match source {
                    ImageSource::Local(bytes) => decode_image(bytes).await,
                    ImageSource::Remote(url) => load_remote_image(path, url).await,
                }
            } => result,
        };
        let _ = send.send(result.map_err(|error| error.to_string()));
        ctx.request_repaint();
    });
    Asset::Loading(recv)
}

async fn load_remote_image(path: PathBuf, url: Url) -> Result<DecodedImage> {
    decode_image(remote_image::load(&path, &url).await?).await
}

async fn decode_image(bytes: Arc<[u8]>) -> Result<DecodedImage> {
    static DECODERS: Semaphore = Semaphore::const_new(4);
    let slot = DECODERS.acquire().await?;
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        let mut reader =
            image::ImageReader::new(Cursor::new(bytes.clone())).with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(64 * 1024 * 1024);
        if reader.format() == Some(image::ImageFormat::Gif) {
            return decode_gif(bytes, limits);
        }
        reader.limits(limits);
        let pixels = reader
            .decode()
            .context("Image format or size is unsupported")?
            .into_rgba8();
        Ok(DecodedImage {
            frames: vec![(color_image(pixels), Duration::ZERO)],
            loops: None,
        })
    })
    .await?
}

fn decode_gif(bytes: Arc<[u8]>, limits: image::Limits) -> Result<DecodedImage> {
    let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))?;
    decoder.set_limits(limits)?;
    let loops = match decoder.loop_count() {
        LoopCount::Infinite => None,
        LoopCount::Finite(count) => Some(count.get()),
    };
    let mut frames = Vec::new();
    let mut total = Duration::ZERO;
    let mut decoded_bytes = 0;
    for frame in decoder.into_frames() {
        let frame = frame?;
        let (ms, denominator) = frame.delay().numer_denom_ms();
        let pixels = frame.into_buffer();
        decoded_bytes += pixels.as_raw().len();
        if !frames.is_empty() && (frames.len() == 120 || decoded_bytes > 64 * 1024 * 1024) {
            frames.truncate(1);
            break;
        }
        total += Duration::from_millis((u64::from(ms) / u64::from(denominator.max(1))).max(20));
        frames.push((color_image(pixels), total));
    }
    ensure!(!frames.is_empty(), "The GIF has no displayable frames");
    Ok(DecodedImage { frames, loops })
}

fn color_image(pixels: image::RgbaImage) -> Arc<ColorImage> {
    Arc::new(ColorImage::from_rgba_unmultiplied(
        [pixels.width() as usize, pixels.height() as usize],
        pixels.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicking_a_glyph_in_a_link_returns_its_target() {
        let ctx = egui::Context::default();
        let runs = [
            Run {
                text: "Ordinary text ".into(),
                style: Default::default(),
            },
            Run {
                text: "Équipe".into(),
                style: message_body::TextStyle {
                    link: Url::parse("https://example.org/checkin").ok(),
                    ..Default::default()
                },
            },
            Run {
                text: " after the link".into(),
                style: Default::default(),
            },
        ];
        let run = |events| {
            let mut target = None;
            let frame = ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        target = inline_text(ui, &runs, Theme::new(true));
                    });
                },
            );
            (frame, target)
        };
        let (frame, _) = run(vec![]);
        let shape = frame
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text().contains("Équipe") => Some(text),
                _ => None,
            })
            .unwrap();
        let row = &shape.galley.rows[0];
        let glyph = row.glyphs.iter().find(|glyph| glyph.chr == 'É').unwrap();
        let pos = shape.pos + row.pos.to_vec2() + glyph.logical_rect().center().to_vec2();
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let _ = run(vec![egui::Event::PointerMoved(pos), button(true)]);
        let (_, target) = run(vec![button(false)]);
        assert_eq!(target.unwrap().as_str(), "https://example.org/checkin");
    }
}
