use std::{
    cmp::Reverse,
    collections::{HashSet, VecDeque},
    path::PathBuf,
};

use super::*;

const CACHE_BYTES: usize = 144 * 1024 * 1024;
const CACHE_MESSAGES: usize = 32;
const WARM_IMAGES: usize = 16;
const WARM_IMAGE_BYTES: usize = 40 * 1024 * 1024;

pub(in crate::ui) type MessageKey = (String, String);

#[derive(Default)]
pub(in crate::ui) struct ReaderContent {
    messages: HashMap<MessageKey, Entry>,
    warming: Option<MessageKey>,
    attempted: HashSet<MessageKey>,
    inbox: HashMap<MessageKey, usize>,
    pending: VecDeque<MessageKey>,
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
                .max_by_key(|(key, entry)| {
                    (
                        entry.speculative,
                        if entry.speculative {
                            self.inbox.get(*key).copied().unwrap_or(0)
                        } else {
                            0
                        },
                        Reverse(entry.last_opened),
                    )
                })
                .filter(|_| self.messages.len() > 1)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            bytes -= self.messages.remove(&key).map_or(0, |entry| entry.bytes());
        }
    }

    pub fn set_inbox(&mut self, keys: Vec<MessageKey>) {
        let inbox: HashMap<_, _> = keys
            .iter()
            .cloned()
            .enumerate()
            .map(|(rank, key)| (key, rank))
            .collect();
        self.messages.retain(|key, entry| {
            inbox.contains_key(key) || (!entry.speculative && !self.inbox.contains_key(key))
        });
        self.inbox = inbox;
        if self
            .warming
            .as_ref()
            .is_some_and(|key| !self.inbox.contains_key(key))
        {
            self.cancel_warm();
        }
        self.attempted.retain(|key| self.inbox.contains_key(key));
        self.pending = keys
            .into_iter()
            .filter(|key| !self.attempted.contains(key))
            .collect();
    }

    pub fn suspend_warming(&mut self) {
        self.cancel_warm();
        self.pending.clear();
    }

    pub fn warm(&mut self, ctx: &egui::Context, runtime: &tokio::runtime::Runtime, path: &Path) {
        if self
            .messages
            .values()
            .any(|entry| matches!(entry.state, State::Loading(_)))
        {
            return;
        }
        while let Some(key) = self.pending.pop_front() {
            if self.attempted.insert(key.clone()) && !self.messages.contains_key(&key) {
                self.warming = Some(key.clone());
                self.start(ctx, runtime, path, &key);
                return;
            }
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
            .is_none_or(|entry| entry.speculative && !matches!(entry.state, State::Ready(_)))
        {
            self.cancel_warm();
            self.start(ui.ctx(), runtime, path, key);
        }
        if let Some(entry) = self.messages.get_mut(key)
            && matches!(entry.state, State::Ready(_))
        {
            entry.speculative = false;
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
            if self.inbox.contains_key(&key) {
                self.pending.push_front(key);
            }
        }
    }

    fn start(
        &mut self,
        ctx: &egui::Context,
        runtime: &tokio::runtime::Runtime,
        path: &Path,
        key: &MessageKey,
    ) {
        let speculative = self.warming.as_ref() == Some(key);
        let (mut send, recv) = oneshot::channel();
        let (ctx, path, key_for_job) = (ctx.clone(), path.to_owned(), key.clone());
        runtime.spawn(async move {
            let result = tokio::select! {
                biased;
                _ = send.closed() => return,
                result = prepare_reader(&ctx, path, key_for_job, speculative) => result,
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
            .chain(reader.attachment_images.values())
            .filter_map(|asset| match asset {
                Asset::Ready(image) => Some(
                    image.decoded.bytes()
                        + image
                            .dark_texture
                            .as_ref()
                            .and_then(Option::as_ref)
                            .map_or(0, |texture| {
                                texture.size().into_iter().product::<usize>() * 4
                            }),
                ),
                _ => None,
            })
            .sum();
        let html = reader
            .html
            .document
            .as_ref()
            .map_or(0, |doc| doc.html.len());
        let attachments: usize = reader
            .attachments
            .iter()
            .map(|(_, bytes)| bytes.len())
            .sum();
        mime + images + html + attachments
    }
}

fn reader_message(store: &Store, key: &MessageKey, raw: Vec<u8>, plain: &str) -> ReaderMessage {
    let body = message_body::from_mime(&raw, plain);
    let view = if body.layout.is_some() {
        store.body_view(&key.0, &key.1).unwrap_or_default()
    } else {
        BodyView::Auto
    };
    let attachments = attached_images(&raw);
    let document = body
        .layout
        .is_some()
        .then(|| cached_document(store, &raw))
        .flatten();
    ReaderMessage {
        attachments,
        attachment_images: HashMap::new(),
        raw: body.layout.is_some().then(|| Arc::from(raw)),
        body,
        view,
        images: HashMap::new(),
        html: HtmlReader {
            document: document.map(Arc::new),
            ..HtmlReader::default()
        },
    }
}

fn attached_images(raw: &[u8]) -> Vec<(usize, Arc<[u8]>)> {
    use mail_parser::MimeHeaders;
    let Some(message) = mail_parser::MessageParser::default().parse(raw) else {
        return Vec::new();
    };
    message
        .attachments()
        .enumerate()
        .filter_map(|(index, part)| {
            let bytes = part.contents();
            let is_image = part
                .content_type()
                .is_some_and(|kind| kind.c_type.eq_ignore_ascii_case("image"))
                || image::guess_format(bytes).is_ok();
            is_image.then(|| (index, Arc::from(bytes)))
        })
        .collect()
}

async fn prepare_reader(
    ctx: &egui::Context,
    path: PathBuf,
    key: MessageKey,
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
    if !speculative {
        return Ok(reader);
    }
    if reader.uses_original_html() {
        if let (Some(raw), Some(document)) = (&reader.raw, &reader.html.document)
            && fetch_html_images(&path, document, Duration::from_millis(1200), WARM_IMAGES).await
        {
            let store = Store::open_read_only(&path)?;
            reader.html.document = cached_document(&store, raw).map(Arc::new);
        }
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
    fn attached_images_preserve_download_indices_and_decoded_bytes() {
        let raw = concat!(
            "MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=parts\r\n\r\n",
            "--parts\r\nContent-Type: text/plain\r\n\r\nBody\r\n",
            "--parts\r\nContent-Type: text/plain\r\n",
            "Content-Disposition: attachment; filename=notes.txt\r\n\r\nNotes\r\n",
            "--parts\r\nContent-Type: image/png\r\n",
            "Content-Disposition: attachment; filename=photo.png\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n",
            "--parts--\r\n"
        );
        let images = attached_images(raw.as_bytes());
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].0, 1);
        assert_eq!(images[0].1.as_ref(), b"hello");
        assert!(attached_images(b"plain message").is_empty());
    }

    #[test]
    fn inbox_preload_cancels_on_archive_and_resumes_on_undo() -> Result<()> {
        use crate::model::Action;

        let mut store = Store::open(Path::new(":memory:"))?;
        let raw = URL_SAFE_NO_PAD.encode("Subject: Test\r\n\r\nInbox item");
        for account in ["home", "cmu"] {
            let messages = (0..20)
                .map(|i| RemoteMessage {
                    id: format!("msg{i}"),
                    thread_id: format!("thread{i}"),
                    label_ids: vec!["INBOX".into()],
                    internal_date: i.to_string(),
                    raw: Some(raw.clone()),
                })
                .collect();
            store.apply_messages(account, 1, messages, &[])?;
        }
        let accounts = ["home", "cmu"];
        let mut cache = ReaderContent::default();
        cache.set_inbox(store.inbox_reader_keys(&accounts)?);
        assert_eq!(cache.pending.len(), 40);

        let key = ("home".to_owned(), "msg19".to_owned());
        let thread = ThreadKey {
            account: key.0.clone(),
            thread: "thread19".into(),
        };
        let (send, recv) = oneshot::channel();
        cache.warming = Some(key.clone());
        cache.attempted.insert(key.clone());
        cache.messages.insert(
            key.clone(),
            Entry {
                state: State::Loading(recv),
                last_opened: Instant::now(),
                speculative: true,
            },
        );

        store.queue_mailbox_changes(
            std::slice::from_ref(&thread),
            &Action::labels(&[], &["INBOX"]),
        )?;
        cache.set_inbox(store.inbox_reader_keys(&accounts)?);
        assert!(send.is_closed());
        assert!(cache.warming.is_none());
        assert_eq!(cache.pending.len(), 39);
        assert!(!cache.messages.contains_key(&key));
        assert!(store.reader_source(&key.0, &key.1, Some(4096))?.is_none());
        assert!(store.reader_source(&key.0, &key.1, None)?.is_some());
        assert!(cache.pending.contains(&("cmu".into(), key.1.clone())));

        store.queue_mailbox_changes(&[thread], &Action::labels(&["INBOX"], &[]))?;
        cache.set_inbox(store.inbox_reader_keys(&accounts)?);
        assert_eq!(cache.pending.len(), 40);
        assert!(cache.pending.contains(&key));
        Ok(())
    }

    #[test]
    fn sanitized_html_and_inline_image_are_ready_before_opening() -> Result<()> {
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
        let mut cache = ReaderContent::default();
        cache.set_inbox(store.inbox_reader_keys(&["home"])?);
        cache.warm(&ctx, &runtime, &path);
        let deadline = Instant::now() + Duration::from_secs(15);
        let document = loop {
            cache.poll(None, &HashSet::new());
            if let Some(reader) = cache.get_mut(&key) {
                let document = reader
                    .html
                    .document
                    .as_ref()
                    .context("Missing HTML document")?;
                assert!(document.html.contains("https://example.invalid/post"));
                assert!(document.html.contains("data:image/png;base64,"));
                assert!(document.missing.is_empty());
                break document.clone();
            }
            ensure!(Instant::now() < deadline, "Reader cache timed out");
            std::thread::sleep(Duration::from_millis(15));
        };
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let opened = cache.open(ui, &runtime, &path, &key).unwrap();
                assert!(Arc::ptr_eq(
                    opened.html.document.as_ref().unwrap(),
                    &document
                ));
            });
        });
        Ok(())
    }
}
