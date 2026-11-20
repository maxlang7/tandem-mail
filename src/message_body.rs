use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD};
use html5ever::{parse_document, tendril::TendrilSink};
use linkify::{LinkFinder, LinkKind};
use mail_parser::{MessageParser, MessagePart, MimeHeaders, PartType};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use reqwest::Url;

pub const IMAGE_LIMIT: usize = 12 * 1024 * 1024;
const HTML_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HtmlLayout {
    Simple,
    Designed,
}

pub struct Body {
    pub blocks: Vec<Block>,
    pub layout: Option<HtmlLayout>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextStyle {
    pub bold: bool,
    pub italic: bool,
    pub mono: bool,
    pub underline: bool,
    pub strike: bool,
    pub heading: u8,
    pub font_size: Option<f32>,
    pub line_height: Option<f32>,
    pub centered: bool,
    pub button: bool,
    pub link: Option<Url>,
}

#[derive(Debug)]
pub struct Run {
    pub text: String,
    pub style: TextStyle,
}

#[derive(Clone, Debug)]
pub enum ImageSource {
    Local(Arc<[u8]>),
    Remote(Url),
}

impl ImageSource {
    pub(crate) fn is_shared_emoji(&self) -> bool {
        matches!(self, Self::Remote(url) if shared_emoji_url(url))
    }
}

#[derive(Debug)]
pub struct TableCell {
    pub width: Option<f32>,
    pub centered: bool,
    pub paragraphs: Vec<Vec<Run>>,
}

#[derive(Debug)]
pub enum Block {
    Text {
        indent: usize,
        marker: Option<String>,
        runs: Vec<Run>,
    },
    TableRow {
        indent: usize,
        cells: Vec<TableCell>,
    },
    Image {
        indent: usize,
        source: ImageSource,
        alt: String,
        width: Option<f32>,
        centered: bool,
        link: Option<Url>,
    },
    Divider,
}

pub fn from_mime(raw: &[u8], plain: &str) -> Body {
    let Some(mail) = MessageParser::default().parse(raw) else {
        return from_plain(plain);
    };
    let mut body = Body {
        blocks: Vec::new(),
        layout: None,
    };
    let embedded = |id: &str| {
        let part = mail
            .parts
            .iter()
            .find(|part| part.content_id() == Some(id))?;
        let bytes = part.contents();
        (bytes.len() <= IMAGE_LIMIT && part.is_binary()).then(|| Arc::from(bytes))
    };
    for html in mail.html_bodies().filter_map(html_part) {
        let dom: RcDom = parse_document(RcDom::default(), Default::default()).one(html);
        body.layout.get_or_insert_with(|| {
            if has_designed_layout(&dom.document, 0) {
                HtmlLayout::Designed
            } else {
                HtmlLayout::Simple
            }
        });
        body.blocks.extend(blocks_from_html(&dom, &embedded));
    }
    if body.blocks.is_empty() {
        body.blocks = from_plain(plain).blocks;
    }
    body
}

pub(crate) fn html_part<'a>(part: &'a MessagePart<'_>) -> Option<&'a str> {
    match &part.body {
        PartType::Html(html) if !html.trim().is_empty() && html.len() <= HTML_LIMIT => Some(html),
        _ => None,
    }
}

fn has_designed_layout(node: &Handle, depth: usize) -> bool {
    if depth >= 128 {
        return false;
    }
    if let NodeData::Element { name, attrs, .. } = &node.data {
        let tag = name.local.as_ref();
        if name.ns.as_ref() != "http://www.w3.org/1999/xhtml"
            || matches!(
                tag,
                "head" | "style" | "script" | "template" | "noscript" | "blockquote"
            )
        {
            return false;
        }
        let attrs = attrs.borrow();
        let attr = |key| {
            attrs
                .iter()
                .find(|a| a.name.local.as_ref() == key)
                .map(|a| a.value.as_ref())
        };
        let css = attr("style").unwrap_or_default();
        let style = |key| css_value(css, key);
        let is = |key, value| style(key).is_some_and(|actual| actual.eq_ignore_ascii_case(value));
        if attr("hidden").is_some()
            || attr("aria-hidden") == Some("true")
            || is("display", "none")
            || is("visibility", "hidden")
        {
            return false;
        }
        let wide = [style("width"), style("max-width"), attr("width")]
            .into_iter()
            .flatten()
            .filter_map(pixels)
            .any(|width| width >= 320.);
        let background = [
            style("background-color"),
            style("background"),
            attr("bgcolor"),
        ]
        .into_iter()
        .flatten()
        .any(|value| {
            !matches!(
                value.to_ascii_lowercase().as_str(),
                "" | "none" | "transparent" | "inherit" | "initial" | "white" | "#fff" | "#ffffff"
            )
        });
        let padded = style("padding").is_some() || style("padding-inline").is_some();
        let centered = is("text-align", "center")
            || attr("align").is_some_and(|align| align.eq_ignore_ascii_case("center"));
        if (tag == "table" && (wide || centered) && (background || wide && (centered || padded)))
            || (tag == "a"
                && background
                && (padded || is("display", "inline-block"))
                && attr("href").and_then(web_url).is_some())
            || (matches!(tag, "div" | "section" | "header" | "footer")
                && background
                && padded
                && (wide || tag == "header" || attr("role") == Some("banner")))
            || (tag == "img" && wide && centered && attr("src").is_some())
            || (is("display", "grid") && style("grid-template-columns").is_some())
        {
            return true;
        }
    }
    node.children
        .borrow()
        .iter()
        .any(|child| has_designed_layout(child, depth + 1))
}

pub fn from_plain(plain: &str) -> Body {
    let plain = plain.replace("\r\n", "\n");
    let mut runs = Vec::new();
    add_links(&mut runs, &plain, &TextStyle::default());
    Body {
        layout: None,
        blocks: vec![Block::Text {
            indent: 0,
            marker: None,
            runs,
        }],
    }
}

fn blocks_from_html(dom: &RcDom, embedded: &dyn Fn(&str) -> Option<Arc<[u8]>>) -> Vec<Block> {
    let mut builder = BodyBuilder::default();
    builder.visit(&dom.document, &TextStyle::default(), 0, false, embedded, 0);
    builder.flush();
    for block in &mut builder.blocks {
        match block {
            Block::Text { runs, .. } => linkify_runs(runs),
            Block::TableRow { cells, .. } => {
                for cell in cells {
                    for runs in &mut cell.paragraphs {
                        linkify_runs(runs);
                    }
                }
            }
            _ => {}
        }
    }
    builder.blocks
}

#[derive(Default)]
struct BodyBuilder {
    blocks: Vec<Block>,
    runs: Vec<Run>,
    indent: usize,
    space: Option<TextStyle>,
    prefix: Option<String>,
    marker: Option<String>,
    trailing_break: bool,
}

impl BodyBuilder {
    fn flush(&mut self) {
        self.space = None;
        if std::mem::take(&mut self.trailing_break)
            && self.runs.iter().any(|run| !run.text.trim().is_empty())
            && let Some(last) = self.runs.last_mut()
            && last.text.ends_with('\n')
        {
            last.text.pop();
            if last.text.is_empty() {
                self.runs.pop();
            }
        }
        if !self.runs.is_empty() {
            self.blocks.push(Block::Text {
                indent: self.indent,
                marker: self.marker.take(),
                runs: std::mem::take(&mut self.runs),
            });
        }
    }

    fn push(&mut self, text: &str, style: &TextStyle, indent: usize) {
        self.trailing_break = false;
        if self.runs.is_empty() {
            self.indent = indent;
            self.marker = self.prefix.take();
        }
        push_run(&mut self.runs, text, style);
    }

    fn text(&mut self, text: &str, style: &TextStyle, indent: usize, pre: bool) {
        if pre {
            self.push(text, style, indent);
            return;
        }
        let mut normalized = String::new();
        for c in text.chars() {
            if c.is_ascii_whitespace() {
                if self.space.is_none() {
                    self.space = Some(style.clone());
                }
            } else {
                if let Some(space_style) = self.space.take() {
                    if !normalized.is_empty() {
                        normalized.push(' ');
                    } else if self
                        .runs
                        .last()
                        .is_some_and(|run| !run.text.ends_with('\n'))
                    {
                        self.push(" ", &space_style, indent);
                    }
                }
                normalized.push(c);
            }
        }
        if !normalized.is_empty() {
            self.push(&normalized, style, indent);
        }
    }

    fn visit(
        &mut self,
        node: &Handle,
        style: &TextStyle,
        indent: usize,
        pre: bool,
        embedded: &dyn Fn(&str) -> Option<Arc<[u8]>>,
        depth: usize,
    ) {
        if depth > 128 {
            return;
        }
        let NodeData::Element { name, attrs, .. } = &node.data else {
            if let NodeData::Text { contents } = &node.data {
                self.text(&contents.borrow(), style, indent, pre);
            } else {
                for child in node.children.borrow().iter() {
                    self.visit(child, style, indent, pre, embedded, depth + 1);
                }
            }
            return;
        };
        let tag = name.local.as_ref();
        if matches!(
            tag,
            "head"
                | "script"
                | "style"
                | "noscript"
                | "template"
                | "iframe"
                | "object"
                | "svg"
                | "canvas"
                | "input"
                | "button"
        ) {
            return;
        }
        let attrs = attrs.borrow();
        let attr = |key| {
            attrs
                .iter()
                .find(|a| a.name.local.as_ref() == key)
                .map(|a| a.value.as_ref())
        };
        let mut next = style.clone();
        if attr("hidden").is_some() || attr("aria-hidden") == Some("true") {
            return;
        }
        let css = attr("style").unwrap_or_default();
        for (property, value) in css_declarations(css) {
            let value = value.to_ascii_lowercase();
            match (property.to_ascii_lowercase().as_str(), value.as_str()) {
                ("display", "none") | ("visibility", "hidden") => return,
                ("font-weight", "bold" | "bolder" | "600" | "700" | "800" | "900") => {
                    next.bold = true
                }
                ("font-style", "italic" | "oblique") => next.italic = true,
                ("font-size", value) => {
                    let size = if let Some(percent) = value.strip_suffix('%') {
                        positive_number(percent)
                            .map(|scale| style.font_size.unwrap_or(14.) * scale / 100.)
                    } else {
                        pixels(value)
                    };
                    if let Some(size) = size {
                        next.font_size = Some(size.clamp(10., 42.));
                    }
                }
                ("line-height", value) => {
                    if let Some(scale) = positive_number(value) {
                        next.line_height = Some(scale.clamp(1., 1.8));
                    }
                }
                ("text-align", value) => next.centered = value == "center",
                ("text-decoration", value) => {
                    next.underline |= value.contains("underline");
                    next.strike |= value.contains("line-through");
                }
                _ => {}
            }
        }
        match tag {
            "br" => {
                self.space = None;
                self.push("\n", &next, indent);
                self.trailing_break = !pre;
                return;
            }
            "hr" => {
                self.flush();
                self.blocks.push(Block::Divider);
                return;
            }
            "img" => {
                if tracking_pixel(css, attr("width"), attr("height"), attr("src")) {
                    return;
                }
                let alt = attr("alt")
                    .filter(|alt| !alt.trim().is_empty())
                    .unwrap_or("Image");
                let source = attr("src").and_then(|src| image_source(src, embedded));
                if let Some(source) = source {
                    let width = css_value(css, "width")
                        .or_else(|| attr("width"))
                        .and_then(pixels)
                        .or_else(|| source.is_shared_emoji().then_some(18.));
                    self.flush();
                    self.blocks.push(Block::Image {
                        indent,
                        source,
                        alt: alt.to_owned(),
                        width,
                        centered: next.centered || attr("align") == Some("center"),
                        link: next.link,
                    });
                } else if alt != "Image" {
                    self.text(alt, &next, indent, pre);
                }
                return;
            }
            "ul" | "ol" => {
                self.flush();
                let mut number = attr("start")
                    .and_then(|number| number.parse::<i32>().ok())
                    .unwrap_or(1);
                for child in node.children.borrow().iter() {
                    if matches!(&child.data, NodeData::Element { name, .. } if name.local.as_ref() == "li")
                    {
                        self.prefix = Some(if tag == "ol" {
                            format!("{number}.")
                        } else {
                            "•".into()
                        });
                        self.visit(child, &next, indent + 1, pre, embedded, depth + 1);
                        self.prefix = None;
                        number = number.saturating_add(1);
                    }
                }
                return;
            }
            "tr" if self.table_row(node, &next, indent, embedded, depth) => return,
            "a" => {
                next.link = attr("href").and_then(web_url);
                next.button = next.link.is_some()
                    && css_value(css, "display") == Some("inline-block")
                    && css_value(css, "background-color").is_some();
            }
            "strong" | "b" | "th" => next.bold = true,
            "em" | "i" => next.italic = true,
            "s" | "strike" | "del" => next.strike = true,
            "u" => next.underline = true,
            "code" | "pre" | "kbd" | "samp" => next.mono = true,
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                next.bold = true;
                next.heading = tag.as_bytes()[1] - b'0';
            }
            _ => {}
        }
        let block = next.heading > 0 && style.heading == 0
            || matches!(
                tag,
                "p" | "div"
                    | "main"
                    | "section"
                    | "article"
                    | "header"
                    | "footer"
                    | "table"
                    | "tr"
                    | "td"
                    | "th"
                    | "pre"
                    | "blockquote"
                    | "li"
                    | "center"
                    | "figure"
            );
        if block {
            self.flush();
        }
        for child in node.children.borrow().iter() {
            self.visit(
                child,
                &next,
                indent + usize::from(tag == "blockquote"),
                pre || tag == "pre",
                embedded,
                depth + 1,
            );
        }
        if block {
            self.flush();
        }
    }
    fn table_row(
        &mut self,
        row: &Handle,
        style: &TextStyle,
        indent: usize,
        embedded: &dyn Fn(&str) -> Option<Arc<[u8]>>,
        depth: usize,
    ) -> bool {
        let children = row.children.borrow();
        let nodes: Vec<_> = children.iter().filter(|node| {
            matches!(&node.data, NodeData::Element { name, .. } if matches!(name.local.as_ref(), "td" | "th"))
        }).collect();
        if !(2..=4).contains(&nodes.len()) || !nodes.iter().all(|cell| text_cell(cell, 0)) {
            return false;
        }
        let mut cells = Vec::new();
        for node in nodes {
            let NodeData::Element { attrs, .. } = &node.data else {
                continue;
            };
            let attrs = attrs.borrow();
            let attr = |key| {
                attrs
                    .iter()
                    .find(|a| a.name.local.as_ref() == key)
                    .map(|a| a.value.as_ref())
            };
            let css = attr("style").unwrap_or_default();
            let mut cell = BodyBuilder::default();
            cell.visit(node, style, 0, false, embedded, depth + 1);
            cell.flush();
            let paragraphs: Vec<_> = cell
                .blocks
                .into_iter()
                .filter_map(|block| match block {
                    Block::Text { runs, .. }
                        if runs.iter().any(|run| !run.text.trim().is_empty()) =>
                    {
                        Some(runs)
                    }
                    _ => None,
                })
                .collect();
            if !paragraphs.is_empty() {
                cells.push(TableCell {
                    width: css_value(css, "width")
                        .or_else(|| attr("width"))
                        .and_then(pixels),
                    centered: css_value(css, "text-align").or_else(|| attr("align"))
                        == Some("center"),
                    paragraphs,
                });
            }
        }
        if cells.len() < 2 {
            return false;
        }
        self.flush();
        self.blocks.push(Block::TableRow { indent, cells });
        true
    }
}

fn text_cell(node: &Handle, depth: usize) -> bool {
    if depth >= 32 {
        return false;
    }
    if let NodeData::Element { name, .. } = &node.data
        && !matches!(
            name.local.as_ref(),
            "td" | "th"
                | "p"
                | "div"
                | "span"
                | "a"
                | "b"
                | "strong"
                | "em"
                | "i"
                | "u"
                | "s"
                | "br"
        )
    {
        return false;
    }
    node.children
        .borrow()
        .iter()
        .all(|child| text_cell(child, depth + 1))
}

fn css_declarations(style: &str) -> impl Iterator<Item = (&str, &str)> {
    style
        .split(';')
        .filter_map(|s| s.split_once(':'))
        .map(|(key, value)| {
            (
                key.trim(),
                value.split('!').next().unwrap_or_default().trim(),
            )
        })
}

fn css_value<'a>(style: &'a str, property: &str) -> Option<&'a str> {
    css_declarations(style)
        .filter_map(|(key, value)| key.eq_ignore_ascii_case(property).then_some(value))
        .last()
}

fn positive_number(value: &str) -> Option<f32> {
    value
        .trim()
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.)
}

fn pixels(value: &str) -> Option<f32> {
    positive_number(value.strip_suffix("px").unwrap_or(value))
}

fn linkify_runs(runs: &mut Vec<Run>) {
    for run in std::mem::take(runs) {
        if run.style.link.is_some() || run.style.mono {
            push_run(runs, &run.text, &run.style);
        } else {
            add_links(runs, &run.text, &run.style);
        }
    }
}

pub(crate) fn web_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.has_host()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

pub(crate) fn image_source(
    raw: &str,
    embedded: &dyn Fn(&str) -> Option<Arc<[u8]>>,
) -> Option<ImageSource> {
    if let Some(id) = raw.strip_prefix("cid:") {
        return embedded(id.trim_matches(['<', '>'])).map(ImageSource::Local);
    }
    if let Some((kind, data)) = raw
        .strip_prefix("data:")
        .and_then(|raw| raw.split_once(','))
        && matches!(
            kind,
            "image/png;base64" | "image/jpeg;base64" | "image/gif;base64" | "image/webp;base64"
        )
        && data.len() <= IMAGE_LIMIT / 3 * 4
    {
        return STANDARD
            .decode(data)
            .ok()
            .map(|bytes| ImageSource::Local(bytes.into()));
    }
    let mut url = web_url(raw).filter(crate::remote_image::public_url)?;
    // Gmail can wrap a Noto emoji in an opaque proxy URL. Use the public asset
    // from its fragment so automatic loading never requests that proxy token.
    if url.path().starts_with("/meips/")
        && url.host_str().is_some_and(|host| {
            host.strip_prefix("ci")
                .and_then(|server| server.strip_suffix(".googleusercontent.com"))
                .is_some_and(|server| {
                    !server.is_empty() && server.bytes().all(|b| b.is_ascii_digit())
                })
        })
        && let Some(emoji) = url.fragment().and_then(web_url).filter(shared_emoji_url)
    {
        return Some(ImageSource::Remote(emoji));
    }
    url.set_fragment(None);
    Some(ImageSource::Remote(url))
}

fn add_links(runs: &mut Vec<Run>, text: &str, style: &TextStyle) {
    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);
    let mut end = 0;
    for link in finder.links(text) {
        if let Some(url) = web_url(link.as_str()) {
            push_run(runs, &text[end..link.start()], style);
            let mut linked = style.clone();
            linked.link = Some(url);
            push_run(runs, link.as_str(), &linked);
            end = link.end();
        }
    }
    push_run(runs, &text[end..], style);
}

fn push_run(runs: &mut Vec<Run>, text: &str, style: &TextStyle) {
    if !text.is_empty() {
        if let Some(last) = runs.last_mut().filter(|run| &run.style == style) {
            last.text.push_str(text);
        } else {
            runs.push(Run {
                text: text.to_owned(),
                style: style.clone(),
            });
        }
    }
}

fn shared_emoji_url(url: &Url) -> bool {
    if url.scheme() != "https"
        || url.host_str() != Some("fonts.gstatic.com")
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let Some(path) = url.path().strip_prefix("/s/e/notoemoji/") else {
        return false;
    };
    let mut parts = path.split('/');
    let (Some(version), Some(codepoint), Some(file), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    version
        .split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        && !codepoint.is_empty()
        && codepoint.len() <= 96
        && codepoint.split('_').all(|part| {
            (1..=6).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_hexdigit())
        })
        && file.strip_suffix(".png").is_some_and(|size| {
            size.parse::<u16>()
                .is_ok_and(|size| (16..=512).contains(&size))
        })
}

pub(crate) fn tracking_pixel(
    style: &str,
    width: Option<&str>,
    height: Option<&str>,
    src: Option<&str>,
) -> bool {
    if src.and_then(web_url).is_some_and(|url| {
        url.host_str() == Some("tartanconnect.cmu.edu")
            && url
                .path()
                .strip_prefix("/read/")
                .and_then(|path| path.strip_suffix(".png"))
                .is_some_and(|id| {
                    id.split('-').map(str::len).eq([8, 4, 4, 4, 12])
                        && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
                })
    }) {
        return true;
    }
    [
        css_value(style, "width").or(width),
        css_value(style, "height").or(height),
    ]
    .into_iter()
    .all(|dimension| {
        dimension.is_some_and(|dimension| {
            let dimension = dimension.trim();
            dimension
                .strip_suffix("px")
                .unwrap_or(dimension)
                .trim()
                .parse::<f32>()
                .is_ok_and(|value| (0. ..=1.).contains(&value))
        })
    })
}

pub fn ed_logo(url: &Url) -> bool {
    url.host_str() == Some("edstem.org") && url.path() == "/email-images/ed-logo.png"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_html(html: &str) -> Body {
        let raw = format!("Content-Type: text/html; charset=utf-8\r\n\r\n{html}");
        from_mime(raw.as_bytes(), "")
    }

    #[test]
    fn auto_layout_keeps_simple_mail_and_signatures_in_the_native_reader() {
        let body = read_html(
            r#"
            <body style="font-family:Arial;background:white">
                <div>Please <strong>check</strong> this:</div>
                <div><a href="https://example.org/wishlist">Wish list</a></div>
                <table width="600" style="width:600px"><tr><td>Kind regards</td></tr></table>
                <img src="https://example.org/pixel.png" width="1" height="1">
                <blockquote><table align="center" width="600"><tr><td>Earlier email</td></tr></table></blockquote>
                <div style="display:none"><table align="center" width="600"><tr><td>Hidden</td></tr></table></div>
            </body>
        "#,
        );
        assert_eq!(body.layout, Some(HtmlLayout::Simple));
        let Block::Text { runs, .. } = &body.blocks[1] else {
            panic!("expected the link")
        };
        assert_eq!(
            runs[0].style.link.as_ref().unwrap().as_str(),
            "https://example.org/wishlist"
        );
        assert_eq!(
            from_mime(b"Content-Type: text/plain\r\n\r\nHello", "Hello").layout,
            None
        );
    }

    #[test]
    fn auto_layout_preserves_wide_cards_and_styled_buttons() {
        for html in [
            r#"<table align="center" style="max-width:600px;width:100%;background-color:#f2f2f2"><tr><td>Ed post</td></tr></table>"#,
            r#"<a style="display:inline-block;background-color:#0070ff;padding:6px 14px" href="https://example.org/post">Open</a>"#,
            r#"<header style="background:#50288c;padding:12px;text-align:center">Newsletter</header>"#,
        ] {
            assert_eq!(read_html(html).layout, Some(HtmlLayout::Designed), "{html}");
        }
    }

    #[test]
    fn detects_tracking_pixels_by_their_dimensions() {
        assert!(tracking_pixel("", Some("1"), Some("1"), None));
        assert!(tracking_pixel("width: 1px; height: 0px", None, None, None));
        assert!(!tracking_pixel(
            "width: 18px; height: 18px",
            Some("1"),
            Some("1"),
            None
        ));
    }

    #[test]
    fn yarnivores_read_pixel_is_omitted_and_proxy_emoji_uses_a_shared_asset() {
        let body = read_html(
            r#"<img src="https://tartanconnect.cmu.edu/read/00000000-0000-0000-0000-000000000001.png" alt="Read receipt">
               <p>Hi Yarnivores,</p>
               <img src="https://ci3.googleusercontent.com/meips/example#https://fonts.gstatic.com/s/e/notoemoji/16.0/1f9f6/72.png" alt="🧶">
               <img src="https://ci3.googleusercontent.com/meips/other#https://example.org/track.png">"#,
        );
        assert_eq!(body.blocks.len(), 3);
        assert!(matches!(body.blocks[0], Block::Text { .. }));
        assert!(
            matches!(&body.blocks[1], Block::Image { source: ImageSource::Remote(url), width: Some(18.), .. }
            if url.host_str() == Some("fonts.gstatic.com"))
        );
        assert!(
            matches!(&body.blocks[2], Block::Image { source: ImageSource::Remote(url), .. }
            if url.host_str() == Some("ci3.googleusercontent.com"))
        );
    }

    #[test]
    fn html_keeps_link_text_nested_lists_and_images_in_reading_order() {
        let html = r#"<p>Open <b><a href="https://example.org/checkin">équipe</a></b> today.</p>
            <div style="display: none !important">hidden</div><script>script</script>
            <ul><li>First<ul><li>Second <img width="641" src="https://static.us.edusercontent.com/example"></li></ul></li></ul>
            <p><a href="javascript:alert(1)">plain words</a></p>"#;
        let blocks = read_html(html).blocks;
        let Block::Text { runs, .. } = &blocks[0] else {
            panic!("expected the first paragraph")
        };
        assert_eq!(
            runs.iter().map(|run| run.text.as_str()).collect::<String>(),
            "Open équipe today."
        );
        let linked = runs.iter().find(|run| run.style.link.is_some()).unwrap();
        assert_eq!(linked.text, "équipe");
        assert!(linked.style.bold);
        assert_eq!(
            linked.style.link.as_ref().unwrap().as_str(),
            "https://example.org/checkin"
        );
        assert!(matches!(blocks[1], Block::Text { indent: 1, .. }));
        assert!(matches!(blocks[2], Block::Text { indent: 2, .. }));
        assert!(
            matches!(&blocks[3], Block::Image { indent: 2, width: Some(641.), source: ImageSource::Remote(url), .. } if url.host_str() == Some("static.us.edusercontent.com"))
        );
        assert!(
            matches!(&blocks[4], Block::Text { runs, .. } if runs.len() == 1 && runs[0].text == "plain words" && runs[0].style.link.is_none())
        );
    }

    #[test]
    fn plain_links_leave_surrounding_punctuation_selectable() {
        let rendered = from_plain("Read (https://example.org/path?q=1), then reply.\r\n").blocks;
        let Block::Text { runs, .. } = &rendered[0] else {
            panic!("expected plain text")
        };
        assert_eq!(runs[0].text, "Read (");
        assert_eq!(
            runs[1].style.link.as_ref().unwrap().as_str(),
            "https://example.org/path?q=1"
        );
        assert_eq!(runs[2].text, "), then reply.\n");
    }

    #[test]
    fn ed_markup_keeps_author_row_button_and_inline_image_dimensions() {
        let html = r#"
            <table><tr><td style="text-align:center"><img width="40" src="https://edstem.org/email-images/ed-logo.png"></td></tr></table>
            <table><tr><td style="width:50px;text-align:center;font-size:24px">D</td>
            <td><div><b>Author</b></div><div>Course · Official</div></td><td></td></tr></table>
            <p><a style="font-size: 120%" href="https://edstem.org/us/courses/1/discussion/2">Check-in steps</a></p>
            <ol><li><p style="line-height:1.4">Open the assignment.<br></p></li></ol>
            <img width="641" style="width:auto;max-width:100%" src="https://static.us.edusercontent.com/example.gif">
            <div><a style="display:inline-block;background-color:#0070ff" href="https://edstem.org/us/courses/1/discussion/2">Open in Ed</a></div>
        "#;
        let blocks = read_html(html).blocks;
        assert!(matches!(
            &blocks[0],
            Block::Image {
                width: Some(40.),
                centered: true,
                ..
            }
        ));
        let Block::TableRow { cells, .. } = &blocks[1] else {
            panic!("expected the author row")
        };
        assert_eq!(cells.len(), 2);
        assert!(cells[0].centered);
        assert_eq!(cells[0].width, Some(50.));
        assert_eq!(cells[0].paragraphs[0][0].style.font_size, Some(24.));
        assert_eq!(cells[1].paragraphs[0][0].text, "Author");
        assert_eq!(cells[1].paragraphs[1][0].text, "Course · Official");
        assert!(
            matches!(&blocks[2], Block::Text { runs, .. } if runs[0].style.font_size.is_some_and(|size| (size - 16.8).abs() < 0.001))
        );
        assert!(
            matches!(&blocks[3], Block::Text { marker: Some(marker), runs, .. } if marker == "1." && runs.len() == 1 && runs[0].text == "Open the assignment." && runs[0].style.line_height == Some(1.4))
        );
        assert!(matches!(&blocks[4], Block::Image { width: None, .. }));
        assert!(
            matches!(&blocks[5], Block::Text { runs, .. } if runs[0].text == "Open in Ed" && runs[0].style.button && runs[0].style.link.is_some())
        );
    }
}
