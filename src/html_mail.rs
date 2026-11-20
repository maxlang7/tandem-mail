use std::{collections::BTreeSet, process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use html5ever::{parse_document, tendril::TendrilSink};
use mail_parser::{MessageParser, MimeHeaders};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Semaphore,
};

use crate::message_body::{self, IMAGE_LIMIT, ImageSource};

const MAX_IMAGES: usize = 24 * 1024 * 1024;
const TILE_HEIGHT: u32 = 1536;
static RENDERER: Semaphore = Semaphore::const_new(1);

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

#[derive(Clone, Copy, Deserialize)]
pub struct Bounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Deserialize)]
pub struct Link {
    pub url: String,
    pub bounds: Bounds,
}

#[derive(Deserialize)]
struct GifBounds {
    index: usize,
    bounds: Bounds,
}

#[derive(Deserialize)]
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
}

pub async fn render(document: Document, width: u32, density: u32) -> Result<Page> {
    let _permit = RENDERER.acquire().await?;
    tokio::time::timeout(
        Duration::from_secs(12),
        render_page(document, width, density),
    )
    .await
    .context("The HTML renderer timed out")?
}

async fn render_page(document: Document, width: u32, density: u32) -> Result<Page> {
    let mut brave = Brave::start()?;
    let result = brave.render(document, width, density).await;
    let _ = brave.child.kill().await;
    result
}

// DevTools drives the isolated browser over private file descriptors. The mail
// document has no access to the pipe, and this worker never navigates to a mail link.
struct Brave {
    child: Child,
    to_browser: ChildStdin,
    from_browser: BufReader<ChildStdout>,
    serial: u32,
    session: Option<String>,
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
        self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width":width,"height":720,"deviceScaleFactor":density,"mobile":false}),
        )
        .await?;
        Ok(())
    }

    async fn render(&mut self, document: Document, width: u32, mut density: u32) -> Result<Page> {
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
        self.call("Network.enable", json!({})).await?;
        self.call(
            "Network.setBlockedURLs",
            json!({"urls":["http://*","https://*","file://*","ftp://*"]}),
        )
        .await?;
        self.set_viewport(width, density).await?;
        self.call("Emulation.setScrollbarsHidden", json!({"hidden":true}))
            .await?;
        self.call(
            "Page.setDocumentContent",
            json!({"frameId":frame["frameTree"]["frame"]["id"], "html":document.html}),
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
        let mut tiles = Vec::new();
        for y in (0..layout.height).step_by(TILE_HEIGHT as usize) {
            let height = TILE_HEIGHT.min(layout.height - y);
            let capture = self.call("Page.captureScreenshot", json!({
                "format":"png", "captureBeyondViewport":true, "clip":{"x":0,"y":y,"width":width,"height":height,"scale":1}
            })).await?;
            let png = STANDARD.decode(
                capture["data"]
                    .as_str()
                    .context("Missing rendered HTML image")?,
            )?;
            tiles.push(
                image::load_from_memory_with_format(&png, image::ImageFormat::Png)?.into_rgba8(),
            );
        }
        Ok(Page {
            width,
            height: layout.height,
            density,
            tiles,
            links: layout.links,
            animated: layout
                .gifs
                .into_iter()
                .filter_map(|gif| Some((gif.bounds, document.animated.get(gif.index)?.clone())))
                .collect(),
            missing: document.missing,
        })
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
}
