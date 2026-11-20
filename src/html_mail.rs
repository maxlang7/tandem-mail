use std::{collections::BTreeSet, process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use html5ever::{parse_document, tendril::TendrilSink};
use mail_parser::{MessageParser, MimeHeaders};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, Semaphore, mpsc, oneshot},
};

use crate::message_body::{self, IMAGE_LIMIT, ImageSource};

pub mod benchmark;
mod raster_cache;
pub use raster_cache::render_cached;

const MAX_IMAGES: usize = 24 * 1024 * 1024;
const TILE_HEIGHT: u32 = 1536;
static RENDERER: Semaphore = Semaphore::const_new(1);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
static WORKER: Mutex<Option<mpsc::Sender<RenderJob>>> = Mutex::const_new(None);

struct RenderJob {
    document: Document,
    width: u32,
    density: u32,
    reply: oneshot::Sender<Result<Page>>,
}

pub struct Document {
    html: String,
    animated: Vec<Arc<[u8]>>,
    pub missing: BTreeSet<Url>,
}

pub fn from_mime(raw: &[u8], cached_image: impl Fn(&Url) -> Option<Vec<u8>>) -> Option<Document> {
    let message = MessageParser::default().parse(raw)?;
    let html = message.html_bodies().find_map(message_body::html_part)?;
    let dom: RcDom = parse_document(RcDom::default(), Default::default()).one(html);
    let mut document = Document {
        html: "<!doctype html><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\"><style>html{color-scheme:light}body{font:14px Arial,sans-serif;overflow-wrap:break-word}img{max-width:calc(100vw - 16px);height:auto}img:not([src]){visibility:hidden!important}</style>".to_owned(),
        animated: Vec::new(),
        missing: BTreeSet::new(),
    };
    let mut remaining = MAX_IMAGES;
    let embedded = |id: &str| {
        let part = message
            .parts
            .iter()
            .find(|part| part.content_id() == Some(id))?;
        let bytes = part.contents();
        (part.is_binary() && bytes.len() <= IMAGE_LIMIT).then(|| Arc::from(bytes))
    };
    for child in dom.document.children.borrow().iter() {
        write_node(
            child,
            &mut document,
            &embedded,
            &cached_image,
            &mut remaining,
            0,
        );
    }
    Some(document)
}

fn write_node(
    node: &Handle,
    document: &mut Document,
    embedded: &dyn Fn(&str) -> Option<Arc<[u8]>>,
    cached: &impl Fn(&Url) -> Option<Vec<u8>>,
    remaining: &mut usize,
    depth: usize,
) {
    if depth >= 128 {
        return;
    }
    match &node.data {
        NodeData::Text { contents } => escape(&mut document.html, &contents.borrow()),
        NodeData::Element { name, attrs, .. } => {
            let tag = name.local.as_ref();
            if name.ns.as_ref() != "http://www.w3.org/1999/xhtml"
                || matches!(
                    tag,
                    "script"
                        | "noscript"
                        | "template"
                        | "plaintext"
                        | "xmp"
                        | "noembed"
                        | "meta"
                        | "base"
                        | "link"
                        | "iframe"
                        | "frame"
                        | "frameset"
                        | "object"
                        | "embed"
                        | "input"
                        | "button"
                        | "select"
                        | "textarea"
                        | "audio"
                        | "video"
                        | "canvas"
                        | "portal"
                        | "title"
                )
            {
                return;
            }
            if tag == "style" {
                let text: String = node
                    .children
                    .borrow()
                    .iter()
                    .filter_map(|child| match &child.data {
                        NodeData::Text { contents } => Some(contents.borrow().to_string()),
                        _ => None,
                    })
                    .collect();
                if !text.contains('<') {
                    document.html.push_str("<style>");
                    document.html.push_str(&text);
                    document.html.push_str("</style>");
                }
                return;
            }
            let attrs = attrs.borrow();
            let attr = |key| {
                attrs
                    .iter()
                    .find(|attr| attr.name.ns.is_empty() && attr.name.local.as_ref() == key)
                    .map(|attr| attr.value.as_ref())
            };
            if tag == "img"
                && message_body::tracking_pixel(
                    attr("style").unwrap_or_default(),
                    attr("width"),
                    attr("height"),
                    attr("src"),
                )
            {
                return;
            }
            let emitted = !matches!(tag, "html" | "head" | "form" | "picture");
            if emitted {
                document.html.push('<');
                document.html.push_str(tag);
                for attr in attrs.iter().filter(|attr| attr.name.ns.is_empty()) {
                    let key = attr.name.local.as_ref();
                    if matches!(
                        key,
                        "style"
                            | "class"
                            | "id"
                            | "align"
                            | "valign"
                            | "width"
                            | "height"
                            | "cellpadding"
                            | "cellspacing"
                            | "colspan"
                            | "rowspan"
                            | "span"
                            | "border"
                            | "bgcolor"
                            | "color"
                            | "size"
                            | "face"
                            | "alt"
                            | "title"
                            | "hidden"
                            | "dir"
                            | "lang"
                            | "role"
                            | "start"
                            | "type"
                            | "reversed"
                            | "nowrap"
                    ) {
                        write_attr(&mut document.html, key, &attr.value);
                    }
                }
                if tag == "a"
                    && let Some(link) = attr("href").and_then(message_body::web_url)
                {
                    write_attr(&mut document.html, "href", link.as_str());
                }
                if tag == "img"
                    && let Some(source) =
                        attr("src").and_then(|raw| message_body::image_source(raw, embedded))
                {
                    if source.is_shared_emoji()
                        && attr("width").is_none()
                        && attr("height").is_none()
                    {
                        write_attr(&mut document.html, "width", "18");
                    }
                    write_image(document, source, cached, remaining);
                }
                document.html.push('>');
            }
            if !matches!(tag, "area" | "br" | "col" | "hr" | "img" | "source" | "wbr") {
                for child in node.children.borrow().iter() {
                    write_node(child, document, embedded, cached, remaining, depth + 1);
                }
                if emitted {
                    document.html.push_str("</");
                    document.html.push_str(tag);
                    document.html.push('>');
                }
            }
        }
        _ => {}
    }
}

fn write_image(
    document: &mut Document,
    source: ImageSource,
    cached: &impl Fn(&Url) -> Option<Vec<u8>>,
    remaining: &mut usize,
) {
    let bytes = match source {
        ImageSource::Local(bytes) => bytes,
        ImageSource::Remote(url) => match cached(&url) {
            Some(bytes) => bytes.into(),
            None => {
                document.missing.insert(url);
                return;
            }
        },
    };
    if bytes.len() > IMAGE_LIMIT || bytes.len() > *remaining {
        return;
    }
    let kind = match image::guess_format(&bytes) {
        Ok(image::ImageFormat::Gif) => "gif",
        Ok(image::ImageFormat::Jpeg) => "jpeg",
        Ok(image::ImageFormat::Png) => "png",
        Ok(image::ImageFormat::WebP) => "webp",
        _ => return,
    };
    *remaining -= bytes.len();
    document.html.push_str(" src=\"data:image/");
    document.html.push_str(kind);
    document.html.push_str(";base64,");
    STANDARD.encode_string(&bytes, &mut document.html);
    document.html.push('"');
    if kind == "gif" && document.animated.len() < 8 {
        write_attr(
            &mut document.html,
            "data-tandem-gif",
            &document.animated.len().to_string(),
        );
        document.animated.push(bytes);
    }
}

fn write_attr(output: &mut String, key: &str, value: &str) {
    output.push(' ');
    output.push_str(key);
    output.push_str("=\"");
    escape(output, value);
    output.push('"');
}

fn escape(output: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            _ => output.push(c),
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub struct Bounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Link {
    pub url: String,
    pub bounds: Bounds,
}

#[derive(Clone, Deserialize, Serialize)]
struct GifBounds {
    index: usize,
    bounds: Bounds,
}

#[derive(Clone, Deserialize, Serialize)]
struct Layout {
    height: u32,
    links: Vec<Link>,
    gifs: Vec<GifBounds>,
}

pub struct Page {
    pub width: u32,
    pub height: u32,
    pub density: u32,
    pub tiles: Vec<image::RgbaImage>,
    pub links: Vec<Link>,
    pub animated: Vec<(Bounds, Arc<[u8]>)>,
    pub missing: BTreeSet<Url>,
    snapshot: Option<Snapshot>,
}

#[derive(Deserialize, Serialize)]
struct SnapshotSize {
    width: u32,
    density: u32,
    layout: Layout,
}

struct Snapshot {
    size: SnapshotSize,
    png: Vec<u8>,
}

impl Snapshot {
    async fn decode(self, document: Document) -> Result<Page> {
        let (snapshot, tiles) = tokio::task::spawn_blocking(move || -> Result<_> {
            let SnapshotSize {
                width,
                density,
                ref layout,
            } = self.size;
            ensure!(
                (1..=1800).contains(&width)
                    && (1..=2).contains(&density)
                    && (1..=8200).contains(&layout.height),
                "Invalid cached viewport"
            );
            let mut reader = image::ImageReader::with_format(
                std::io::Cursor::new(&self.png),
                image::ImageFormat::Png,
            );
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(width * density);
            limits.max_image_height = Some(layout.height * density);
            limits.max_alloc = Some(128 * 1024 * 1024);
            reader.limits(limits);
            let pixels = reader.decode()?.into_rgba8();
            ensure!(
                pixels.width() == width * density && pixels.height() == layout.height * density,
                "Unexpected HTML capture size"
            );
            let tile_height = TILE_HEIGHT * density;
            let tiles = if pixels.height() <= tile_height {
                vec![pixels]
            } else {
                pixels
                    .as_raw()
                    .chunks((pixels.width() * tile_height * 4) as usize)
                    .map(|bytes| {
                        image::RgbaImage::from_raw(
                            pixels.width(),
                            bytes.len() as u32 / (pixels.width() * 4),
                            bytes.to_vec(),
                        )
                        .unwrap()
                    })
                    .collect()
            };
            Ok((self, tiles))
        })
        .await??;
        Ok(Page {
            width: snapshot.size.width,
            height: snapshot.size.layout.height,
            density: snapshot.size.density,
            tiles,
            links: snapshot.size.layout.links.clone(),
            animated: snapshot
                .size
                .layout
                .gifs
                .iter()
                .filter_map(|gif| Some((gif.bounds, document.animated.get(gif.index)?.clone())))
                .collect(),
            missing: document.missing,
            snapshot: Some(snapshot),
        })
    }
}

pub async fn render(document: Document, width: u32, density: u32) -> Result<Page> {
    ensure!(
        (1..=1800).contains(&width) && (1..=2).contains(&density),
        "Unsupported HTML viewport"
    );
    let _permit = RENDERER.acquire().await?;
    let (reply, result) = oneshot::channel();
    let mut job = RenderJob {
        document,
        width,
        density,
        reply,
    };
    loop {
        let sender = {
            let mut worker = WORKER.lock().await;
            if worker.as_ref().is_none_or(mpsc::Sender::is_closed) {
                let (send, receive) = mpsc::channel(1);
                *worker = Some(send);
                tokio::spawn(render_worker(receive));
            }
            worker.as_ref().unwrap().clone()
        };
        match sender.send(job).await {
            Ok(()) => break,
            // The idle deadline can expire between cloning and sending.
            Err(error) => job = error.0,
        }
    }
    result.await.context("The HTML renderer stopped")?
}

async fn render_worker(mut jobs: mpsc::Receiver<RenderJob>) {
    let mut browser: Option<Brave> = None;
    while let Ok(Some(mut job)) = tokio::time::timeout(IDLE_TIMEOUT, jobs.recv()).await {
        if job.reply.is_closed() {
            continue;
        }
        if browser.is_none() {
            match Brave::start() {
                Ok(brave) => browser = Some(brave),
                Err(error) => {
                    let _ = job.reply.send(Err(error));
                    continue;
                }
            }
        }
        let brave = browser.as_mut().unwrap();
        let result = tokio::select! {
            biased;
            // A canceled preload must not hold up the message the user clicked.
            _ = job.reply.closed() => { browser = None; continue; }
            result = tokio::time::timeout(Duration::from_secs(12), brave.render(job.document, job.width, job.density)) => {
                result.context("The HTML renderer timed out").and_then(|result| result)
            }
        };
        // Never reuse a partially completed CDP conversation after a failure.
        if result.is_err() {
            browser = None;
        }
        let _ = job.reply.send(result);
    }
    if let Some(mut brave) = browser {
        let _ = brave.child.kill().await;
    }
}

// DevTools drives the isolated browser over private file descriptors. The mail
// document has no access to the pipe, and this worker never navigates to a mail link.
struct Brave {
    child: Child,
    to_browser: ChildStdin,
    from_browser: BufReader<ChildStdout>,
    serial: u32,
    session: Option<String>,
    frame: Option<String>,
    viewport: Option<(u32, u32)>,
    _profile: tempfile::TempDir,
}

impl Brave {
    fn start() -> Result<Self> {
        let profile = tempfile::Builder::new().prefix("tandem-html-").tempdir()?;
        let mut child = Command::new("sh")
            .args([
                "-c",
                "exec 3<&0 4>&1; exec </dev/null >/dev/null; if command -v brave >/dev/null; then exec brave \"$@\"; fi; exec brave-browser \"$@\"",
                "tandem-html",
                "--headless",
                "--no-startup-window",
                "--disable-gpu",
                "--remote-debugging-pipe",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-sync",
                "--host-resolver-rules=MAP * ~NOTFOUND",
            ])
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("Could not start Brave for HTML mail")?;
        let to_browser = child.stdin.take().context("Missing Brave input pipe")?;
        let from_browser =
            BufReader::new(child.stdout.take().context("Missing Brave output pipe")?);
        Ok(Self {
            child,
            to_browser,
            from_browser,
            serial: 0,
            session: None,
            frame: None,
            viewport: None,
            _profile: profile,
        })
    }

    async fn call(&mut self, name: &str, params: Value) -> Result<Value> {
        self.serial += 1;
        let mut request = json!({"id":self.serial,"method":name,"params":params});
        if let Some(session) = &self.session {
            request["sessionId"] = json!(session);
        }
        let mut request = serde_json::to_vec(&request)?;
        request.push(0);
        self.to_browser.write_all(&request).await?;
        let mut response = Vec::new();
        loop {
            response.clear();
            ensure!(
                self.from_browser.read_until(0, &mut response).await? > 0,
                "Brave closed the renderer"
            );
            ensure!(response.pop() == Some(0), "Incomplete renderer response");
            let value: Value = serde_json::from_slice(&response)?;
            if value["id"].as_u64() == Some(self.serial.into()) {
                if let Some(error) = value.get("error") {
                    bail!("{name}: {}", error["message"]);
                }
                return Ok(value["result"].clone());
            }
        }
    }

    async fn set_viewport(&mut self, width: u32, density: u32) -> Result<()> {
        if self.viewport == Some((width, density)) {
            return Ok(());
        }
        self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width":width,"height":720,"deviceScaleFactor":density,"mobile":false}),
        )
        .await?;
        self.viewport = Some((width, density));
        Ok(())
    }

    async fn initialize(&mut self) -> Result<()> {
        if self.session.is_some() {
            return Ok(());
        }
        let target = self
            .call("Target.createTarget", json!({"url":"about:blank"}))
            .await?;
        let attach = self
            .call(
                "Target.attachToTarget",
                json!({"targetId": target["targetId"],"flatten": true}),
            )
            .await?;
        self.session = Some(
            attach["sessionId"]
                .as_str()
                .context("Missing Brave session")?
                .to_owned(),
        );
        let frame = self.call("Page.getFrameTree", json!({})).await?;
        self.frame = Some(
            frame["frameTree"]["frame"]["id"]
                .as_str()
                .context("Missing HTML frame")?
                .to_owned(),
        );
        self.call("Network.enable", json!({})).await?;
        self.call(
            "Network.setBlockedURLs",
            json!({"urls":["http://*","https://*","file://*","ftp://*"]}),
        )
        .await?;
        self.call("Emulation.setScrollbarsHidden", json!({"hidden":true}))
            .await?;
        Ok(())
    }

    async fn render(&mut self, document: Document, width: u32, mut density: u32) -> Result<Page> {
        self.initialize().await?;
        self.call("Page.setWebLifecycleState", json!({"state":"active"}))
            .await?;
        self.set_viewport(width, density).await?;
        self.call(
            "Page.setDocumentContent",
            json!({"frameId":self.frame, "html":document.html}),
        )
        .await?;
        let result = self
            .call(
                "Runtime.evaluate",
                json!({"expression":MEASURE_PAGE, "awaitPromise":true, "returnByValue":true}),
            )
            .await?;
        let layout: Layout = serde_json::from_value(result["result"]["value"].clone())
            .context("Could not measure HTML mail")?;
        ensure!(
            (1..=8200).contains(&layout.height),
            "HTML mail is too tall for the inline reader"
        );
        if width * layout.height * density.pow(2) > 16_000_000 {
            density = 1;
            self.set_viewport(width, density).await?;
        }
        // One compositor capture avoids waiting for a new frame for each tile.
        // Fast PNG encoding is lossless; texture tiling happens after decoding.
        let capture = self
            .call(
                "Page.captureScreenshot",
                json!({
                    "format":"png", "optimizeForSpeed":true, "captureBeyondViewport":true,
                    "clip":{"x":0,"y":0,"width":width,"height":layout.height,"scale":1}
                }),
            )
            .await?;
        self.call("Page.setWebLifecycleState", json!({"state":"frozen"}))
            .await?;
        let png = STANDARD.decode(
            capture["data"]
                .as_str()
                .context("Missing rendered HTML image")?,
        )?;
        Snapshot {
            size: SnapshotSize {
                width,
                density,
                layout,
            },
            png,
        }
        .decode(document)
        .await
    }
}

const MEASURE_PAGE: &str = r#"
(async () => {
    await Promise.all(Array.from(document.images, image => image.decode().catch(() => {})));
    const visible = node => {
        const style = getComputedStyle(node);
        return style.visibility === 'visible' && Number(style.opacity) > 0;
    };
    const shape = ({x, y, width, height}) => ({x, y, width, height});
    const hasArea = ({x, y, width, height}) => width > 0 && height > 0 && x + width > 0 && y + height > 0;
    const links = Array.from(document.querySelectorAll('a[href]'))
        .filter(visible)
        .flatMap(a => Array.from(a.getClientRects(), shape)
            .filter(hasArea).map(bounds => ({url:a.getAttribute('href'), bounds})));
    const gifs = Array.from(document.querySelectorAll('img[data-tandem-gif]'))
        .filter(image => image.naturalWidth > 0 && visible(image))
        .map(image => {
            const bounds = shape(image.getBoundingClientRect());
            image.style.setProperty('visibility', 'hidden', 'important');
            return {index:Number(image.dataset.tandemGif), bounds};
        }).filter(gif => hasArea(gif.bounds));
    const body = document.body;
    const root = document.documentElement;
    const height = Math.ceil(Math.max(1, body.getBoundingClientRect().bottom, body.scrollHeight,
        root.scrollHeight > innerHeight ? root.scrollHeight : 0));
    return {height, links:links.slice(0, 2000), gifs};
})()
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_html() -> &'static [u8] {
        br#"Content-Type: text/html; charset=utf-8

<html><head><meta http-equiv="refresh" content="0;url=https://example.org/"></head>
<body style="margin:0;background:#f2f2f2">
<table align="center" style="width:600px;background:#50288c"><tr><td>
<a href="https://example.org/thread" style="color:white">Open in Ed</a>
</td></tr></table>
<table align="center" width="600"><tr><td>
<img src="https://example.org/banner.png" style="height:14px;display:block" srcset="https://example.org/track.png" onerror="steal()">
</td><td width="100%">Account</td></tr></table>
<img src="https://example.org/tracking-pixel.gif" width="1" height="1" alt="tracking-pixel">
<img src="https://tartanconnect.cmu.edu/read/00000000-0000-0000-0000-000000000001.png" alt="read-receipt">
<script>steal()</script><iframe src="https://example.org/frame"></iframe>
<a href="javascript:steal()">Text</a>
</body></html>"#
    }

    #[test]
    fn preserves_mail_css_and_only_inlines_images_from_the_cache() {
        let page = from_mime(sample_html(), |_| None).unwrap();
        assert!(page.html.contains("width:600px;background:#50288c"));
        assert!(page.html.contains("https://example.org/thread"));
        assert!(page.html.contains("default-src 'none'"));
        assert_eq!(
            page.missing,
            BTreeSet::from([Url::parse("https://example.org/banner.png").unwrap()])
        );
        for filtered in [
            "steal",
            "srcset",
            "iframe",
            "http-equiv=\"refresh\"",
            "https://example.org/banner.png",
            "tracking-pixel",
            "read-receipt",
        ] {
            assert!(
                !page.html.contains(filtered),
                "{filtered} survived sanitization"
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires the local Brave browser"]
    async fn renders_centered_css_table_with_clickable_link_geometry() -> Result<()> {
        let logo_color = image::Rgba([235, 31, 173, 255]);
        let logo = image::RgbaImage::from_pixel(40, 20, logo_color);
        let mut encoded = std::io::Cursor::new(Vec::new());
        logo.write_to(&mut encoded, image::ImageFormat::Png)?;
        let document = from_mime(sample_html(), |_| Some(encoded.get_ref().clone())).unwrap();
        let page = render(document, 1000, 2).await?;
        let image = &page.tiles[0];
        assert_eq!(image.width(), 2000);
        assert_eq!(image.get_pixel(50, 16).0, [242, 242, 242, 255]);
        assert_eq!(image.get_pixel(1500, 16).0, [80, 40, 140, 255]);
        assert_eq!(page.links.len(), 1);
        assert_eq!(page.links[0].url, "https://example.org/thread");
        assert!((200. ..215.).contains(&page.links[0].bounds.x));
        assert!(
            image.pixels().any(|pixel| *pixel == logo_color),
            "The table squeezed the logo out of view"
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires the local Brave browser"]
    async fn reused_page_replaces_content_and_matches_tiled_capture() -> Result<()> {
        let mut brave = Brave::start()?;
        let first = from_mime(sample_html(), |_| None).unwrap();
        brave.render(first, 1000, 2).await?;
        let session = brave.session.clone();
        let raw = b"Content-Type: text/html\r\n\r\n<body style=\"margin:0;background:#1b764e\"><a href=\"https://example.org/new\">New document</a><div style=\"height:3600px\"></div><p>Last tile</p></body>";
        let page = brave
            .render(from_mime(raw, |_| None).unwrap(), 800, 1)
            .await?;
        assert_eq!(brave.session, session);
        assert_eq!(page.links.len(), 1);
        assert_eq!(page.links[0].url, "https://example.org/new");
        assert_eq!(page.tiles.len(), 3);
        // Compare every pixel with the previous per-tile screenshot method.
        brave
            .call("Page.setWebLifecycleState", json!({"state":"active"}))
            .await?;
        for (tile, y) in page
            .tiles
            .iter()
            .zip((0..page.height).step_by(TILE_HEIGHT as usize))
        {
            let capture = brave
                .call(
                    "Page.captureScreenshot",
                    json!({
                        "format":"png", "captureBeyondViewport":true,
                        "clip":{"x":0,"y":y,"width":800,"height":tile.height(),"scale":1}
                    }),
                )
                .await?;
            let png = STANDARD.decode(capture["data"].as_str().unwrap())?;
            let reference =
                image::load_from_memory_with_format(&png, image::ImageFormat::Png)?.into_rgba8();
            assert_eq!(tile, &reference, "Pixels differ on tile at {y}");
        }
        brave.child.kill().await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires the local Brave browser; checks real idle shutdown"]
    async fn worker_recovers_from_cancellation_and_shuts_down_when_idle() -> Result<()> {
        let (sender, receiver) = mpsc::channel(1);
        let worker = tokio::spawn(render_worker(receiver));
        let (reply, receive) = oneshot::channel();
        sender
            .send(RenderJob {
                document: from_mime(sample_html(), |_| None).unwrap(),
                width: 800,
                density: 1,
                reply,
            })
            .await
            .map_err(|_| anyhow::anyhow!("Worker closed"))?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(receive);
        let (reply, receive) = oneshot::channel();
        sender
            .send(RenderJob {
                document: from_mime(sample_html(), |_| None).unwrap(),
                width: 1000,
                density: 2,
                reply,
            })
            .await
            .map_err(|_| anyhow::anyhow!("Worker closed"))?;
        let page = tokio::time::timeout(Duration::from_secs(12), receive).await???;
        assert_eq!(page.width, 1000);
        assert_eq!(page.density, 2);
        assert_eq!(page.links.len(), 1);
        tokio::time::timeout(IDLE_TIMEOUT + Duration::from_secs(2), worker).await??;
        assert!(sender.is_closed(), "Idle renderer stayed alive");
        Ok(())
    }
}
