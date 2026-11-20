use std::{collections::HashSet, path::PathBuf};

use super::*;

const CACHE_BYTES: usize = 144 * 1024 * 1024;
const CACHE_MESSAGES: usize = 32;
const WARM_IMAGES: usize = 16;
const WARM_IMAGE_BYTES: usize = 40 * 1024 * 1024;

pub(in crate::ui) type MessageKey = (String, String);

#[derive(Clone, Copy)]
pub(in crate::ui) struct RenderSize {
    pub width: u32,
    pub density: u32,
}

impl RenderSize {
    pub fn new(ctx: &egui::Context, width: f32) -> Self {
        Self {
            width: (width.clamp(200., 1800.) as u32 / 8) * 8,
            density: ctx.pixels_per_point().ceil().clamp(1., 2.) as u32,
        }
    }
}

#[derive(Default)]
pub(in crate::ui) struct ReaderContent {
    messages: HashMap<MessageKey, Entry>,
    warming: Option<MessageKey>,
    attempted: HashSet<MessageKey>,
    start_after: Option<Instant>,
}

struct Entry {
    state: State,
    last_opened: Instant,
    speculative: bool,
}

enum State {
    Loading(oneshot::Receiver<Option<Box<ReaderMessage>>>),
    Ready(Box<ReaderMessage>),
    Unavailable,
}

impl ReaderContent {
    pub fn poll(&mut self, account: Option<&str>, expanded: &HashSet<String>) {
        for entry in self.messages.values_mut() {
            if let State::Loading(job) = &mut entry.state {
                entry.state = match job.try_recv() {
                    Ok(Some(reader)) => State::Ready(reader),
                    Ok(None) | Err(oneshot::error::TryRecvError::Closed) => State::Unavailable,
                    Err(oneshot::error::TryRecvError::Empty) => continue,
                };
            }
        }
        self.messages.retain(|key, entry| {
            !matches!(entry.state, State::Loading(_))
                || self.warming.as_ref() == Some(key)
                || (account == Some(key.0.as_str()) && expanded.contains(&key.1))
        });
        if self.warming.as_ref().is_some_and(|key| {
            !matches!(
                self.messages.get(key).map(|entry| &entry.state),
                Some(State::Loading(_))
            )
        }) {
            self.warming = None;
        }
        let mut bytes: usize = self.messages.values().map(Entry::bytes).sum();
        while self.messages.len() > CACHE_MESSAGES || bytes > CACHE_BYTES {
            let Some(key) = self
                .messages
                .iter()
                .filter(|(_, entry)| !matches!(entry.state, State::Loading(_)))
                .min_by_key(|(_, entry)| entry.last_opened)
                .filter(|_| self.messages.len() > 1)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            bytes -= self.messages.remove(&key).map_or(0, |entry| entry.bytes());
        }
    }

    pub fn warm(
        &mut self,
        ctx: &egui::Context,
        runtime: &tokio::runtime::Runtime,
        path: &Path,
        candidates: &[MessageKey],
        size: RenderSize,
    ) {
        if candidates.is_empty() {
            self.cancel_warm();
            self.attempted.clear();
            return;
        }
        let start_after = *self
            .start_after
            .get_or_insert_with(|| Instant::now() + Duration::from_millis(200));
        if let Some(delay) = start_after.checked_duration_since(Instant::now()) {
            ctx.request_repaint_after(delay);
            return;
        }
        self.attempted.retain(|key| candidates.contains(key));
        if self
            .warming
            .as_ref()
            .is_some_and(|key| !candidates.contains(key))
        {
            self.cancel_warm();
        }
        if self
            .messages
            .values()
            .any(|entry| matches!(entry.state, State::Loading(_)))
        {
            return;
        }
        if let Some(key) = candidates
            .iter()
            .find(|key| !self.messages.contains_key(*key) && !self.attempted.contains(*key))
        {
            self.attempted.insert(key.clone());
            self.warming = Some(key.clone());
            self.start(ctx, runtime, path, key, size);
        }
    }

    pub(super) fn open(
        &mut self,
        ui: &Ui,
        runtime: &tokio::runtime::Runtime,
        path: &Path,
        key: &MessageKey,
    ) -> Option<&mut ReaderMessage> {
        if self
            .messages
            .get(key)
            .is_none_or(|entry| entry.speculative && matches!(entry.state, State::Unavailable))
        {
            self.cancel_warm();
            self.start(
                ui.ctx(),
                runtime,
                path,
                key,
                RenderSize::new(ui.ctx(), ui.available_width()),
            );
        } else if self.warming.as_ref() == Some(key) {
            self.warming = None;
        }
        self.get_mut(key)
    }

    pub(super) fn open_demo(
        &mut self,
        store: &Store,
        key: &MessageKey,
        plain: &str,
    ) -> Option<&mut ReaderMessage> {
        self.messages.entry(key.clone()).or_insert_with(|| {
            let (raw, plain) = store
                .reader_source(&key.0, &key.1, None)
                .ok()
                .flatten()
                .unwrap_or_else(|| (vec![], plain.to_owned()));
            Entry {
                state: State::Ready(Box::new(reader_message(store, key, raw, &plain))),
                last_opened: Instant::now(),
                speculative: false,
            }
        });
        self.get_mut(key)
    }

    pub(super) fn get_mut(&mut self, key: &MessageKey) -> Option<&mut ReaderMessage> {
        let entry = self.messages.get_mut(key)?;
        entry.last_opened = Instant::now();
        match &mut entry.state {
            State::Ready(reader) => Some(reader),
            _ => None,
        }
    }

    fn cancel_warm(&mut self) {
        if let Some(key) = self.warming.take() {
            self.messages.remove(&key);
            self.attempted.remove(&key);
        }
    }

    fn start(
        &mut self,
        ctx: &egui::Context,
        runtime: &tokio::runtime::Runtime,
        path: &Path,
        key: &MessageKey,
        size: RenderSize,
    ) {
        let speculative = self.warming.as_ref() == Some(key);
        let (mut send, recv) = oneshot::channel();
        let (ctx, path, key_for_job) = (ctx.clone(), path.to_owned(), key.clone());
        runtime.spawn(async move {
            let result = tokio::select! {
                _ = send.closed() => return,
                result = prepare_reader(&ctx, path, key_for_job, size, speculative) => result,
            };
            let _ = send.send(result.ok().map(Box::new));
            ctx.request_repaint();
        });
        self.messages.insert(
            key.clone(),
            Entry {
                state: State::Loading(recv),
                last_opened: Instant::now(),
                speculative,
            },
        );
    }
}

impl Entry {
    fn bytes(&self) -> usize {
        let State::Ready(reader) = &self.state else {
            return 0;
        };
        // Inline MIME bytes can also appear in parsed blocks.
        let mime = reader
            .raw
            .as_ref()
            .map_or(0, |raw| raw.len().saturating_mul(2));
        let images: usize = reader
            .images
            .values()
            .filter_map(|asset| match asset {
                Asset::Ready(image) => Some(image.decoded.bytes()),
                _ => None,
            })
            .sum();
        let html = reader.html.page.as_ref().map_or(0, |page| {
            page.tiles
                .iter()
                .map(|tile| tile.size().into_iter().product::<usize>() * 4)
                .sum::<usize>()
                + page
                    .gifs
                    .iter()
                    .map(|(_, image)| image.decoded.bytes())
                    .sum::<usize>()
        });
        mime + images + html
    }
}

fn reader_message(store: &Store, key: &MessageKey, raw: Vec<u8>, plain: &str) -> ReaderMessage {
    let body = message_body::from_mime(&raw, plain);
    let view = if body.layout.is_some() {
        store.body_view(&key.0, &key.1).unwrap_or_default()
    } else {
        BodyView::Auto
    };
    ReaderMessage {
        raw: body.layout.is_some().then(|| Arc::from(raw)),
        body,
        view,
        images: HashMap::new(),
        html: HtmlReader::default(),
    }
}

async fn prepare_reader(
    ctx: &egui::Context,
    path: PathBuf,
    key: MessageKey,
    size: RenderSize,
    speculative: bool,
) -> Result<ReaderMessage> {
    let (source_path, source_key) = (path.clone(), key.clone());
    let mut reader = tokio::task::spawn_blocking(move || {
        let store = Store::open_read_only(&source_path)?;
        let (raw, plain) = store
            .reader_source(
                &source_key.0,
                &source_key.1,
                speculative.then_some(16 * 1024 * 1024),
            )?
            .context("Message is unavailable for preparation")?;
        Ok::<_, anyhow::Error>(reader_message(&store, &source_key, raw, &plain))
    })
    .await??;
    if reader.uses_original_html()
        && let Some(raw) = &reader.raw
    {
        match PaintedPage::prepare(ctx, path.clone(), raw.clone(), size, false, &key.1).await {
            Ok(Some(page)) => {
                reader.html.page = Some(page);
                reader.html.width = size.width;
                reader.html.density = size.density;
                return Ok(reader);
            }
            Ok(None) => reader.html.stopped = true,
            Err(error) => {
                eprintln!("HTML reader: {error}");
                reader.html.stopped = true;
            }
        }
    }
    if !speculative {
        return Ok(reader);
    }
    let sources: Vec<_> = reader
        .body
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| match block {
            Block::Image { source, .. } => Some((index, source.clone())),
            _ => None,
        })
        .take(WARM_IMAGES)
        .collect();
    let mut images = stream::iter(sources)
        .map(|(index, source)| {
            let path = path.clone();
            async move {
                let pixels = match source {
                    ImageSource::Local(bytes) => decode_image(bytes).await?,
                    ImageSource::Remote(url) => load_remote_image(path, url).await?,
                };
                Ok::<_, anyhow::Error>((index, pixels))
            }
        })
        .buffer_unordered(3);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1200);
    let mut bytes = 0;
    while let Ok(Some(result)) = tokio::time::timeout_at(deadline, images.next()).await {
        if let Ok((index, pixels)) = result {
            bytes += pixels.bytes();
            if bytes > WARM_IMAGE_BYTES {
                break;
            }
            let texture = AnimatedImage::new(ctx, format!("mail:{}:{index}", key.1), pixels);
            reader.images.insert(index, Asset::Ready(texture));
        }
    }
    Ok(reader)
}

#[cfg(test)]
mod tests {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    use super::*;
    use crate::model::RemoteMessage;

    #[test]
    #[ignore = "requires the local Brave browser"]
    fn visible_html_and_inline_image_are_ready_before_opening() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("mail.sqlite");
        let mut store = Store::open(&path)?;
        let mut png = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(48, 24, image::Rgba([39, 181, 96, 255]))
            .write_to(&mut png, image::ImageFormat::Png)?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(png.into_inner());
        let mime = format!(
            "Content-Type: text/html; charset=utf-8\r\n\r\n\
             <table style=\"width:420px;background-color:#4c2584\"><tr><td>\
             <a href=\"https://example.invalid/post\">Open post</a>\
             <img src=\"data:image/png;base64,{encoded}\">\
             <img src=\"https://example.invalid/external.png\">\
             </td></tr></table>"
        );
        store.apply_messages(
            "home",
            1,
            vec![RemoteMessage {
                id: "msg1".into(),
                thread_id: "thread1".into(),
                label_ids: vec!["INBOX".into()],
                internal_date: crate::store::now().to_string(),
                raw: Some(URL_SAFE_NO_PAD.encode(mime)),
            }],
            &[],
        )?;

        let ctx = egui::Context::default();
        let runtime = tokio::runtime::Runtime::new()?;
        let key = ("home".to_owned(), "msg1".to_owned());
        let size = RenderSize::new(&ctx, 600.);
        let mut cache = ReaderContent {
            start_after: Some(Instant::now()),
            ..ReaderContent::default()
        };
        cache.warm(&ctx, &runtime, &path, std::slice::from_ref(&key), size);
        let deadline = Instant::now() + Duration::from_secs(15);
        let tile = loop {
            cache.poll(None, &HashSet::new());
            if let Some(reader) = cache.get_mut(&key) {
                let page = reader
                    .html
                    .page
                    .as_ref()
                    .context("HTML page was not prepared")?;
                assert_eq!(page.links.len(), 1);
                assert!(page.missing_images);
                break page.tiles[0].id();
            }
            ensure!(Instant::now() < deadline, "Reader cache timed out");
            std::thread::sleep(Duration::from_millis(15));
        };
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let opened = cache.open(ui, &runtime, &path, &key).unwrap();
                assert_eq!(opened.html.page.as_ref().unwrap().tiles[0].id(), tile);
            });
        });
        assert!(output.textures_delta.set.iter().any(|(id, delta)| {
            *id == tile
                && match &delta.image {
                    ImageData::Color(pixels) => pixels
                        .pixels
                        .contains(&egui::Color32::from_rgb(39, 181, 96)),
                }
        }));
        assert!(
            store
                .remote_image("https://example.invalid/external.png")?
                .is_none()
        );
        Ok(())
    }
}
