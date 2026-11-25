use std::ops::Range;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub link: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Span {
    pub chars: Range<usize>,
    pub style: Style,
}

pub fn runs<'a>(
    text: &'a str,
    spans: &'a [Span],
) -> impl Iterator<Item = (&'a str, Option<&'a Style>)> {
    let mut remaining = text.char_indices();
    let mut character = 0;
    let mut byte = 0;
    let mut span = 0;
    std::iter::from_fn(move || {
        if byte == text.len() {
            return None;
        }
        while spans.get(span).is_some_and(|s| s.chars.end <= character) {
            span += 1;
        }
        let (end, style) = match spans.get(span) {
            Some(s) if s.chars.start <= character => (s.chars.end, Some(&s.style)),
            Some(s) => (s.chars.start, None),
            None => (usize::MAX, None),
        };
        let next = if end == usize::MAX {
            text.len()
        } else {
            // The iterator still includes the character at the previous boundary.
            remaining
                .nth(end - character - 1)
                .map_or(text.len(), |(i, c)| i + c.len_utf8())
        };
        let result = (&text[byte..next], style);
        character = end;
        byte = next;
        Some(result)
    })
}

pub fn link_url(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return None;
    }
    let url = reqwest::Url::parse(value)
        .or_else(|_| reqwest::Url::parse(&format!("https://{value}")))
        .ok()?;
    match url.scheme() {
        "http" | "https" if url.host().is_some() => Some(url.into()),
        "mailto" if !url.path().is_empty() => Some(url.into()),
        _ => None,
    }
}

pub fn html(text: &str, spans: &[Span]) -> String {
    let mut html = String::from("<div style=\"white-space: pre-wrap\">");
    for (text, style) in runs(text, spans) {
        let mut tags = Vec::new();
        if let Some(style) = style {
            if let Some(url) = style.link.as_deref().and_then(link_url) {
                html.push_str("<a href=\"");
                escape(&mut html, &url);
                html.push_str("\">");
                tags.push("a");
            }
            for (enabled, tag) in [
                (style.bold, "strong"),
                (style.italic, "em"),
                (style.underline, "u"),
                (style.strike, "s"),
            ] {
                if enabled {
                    html.push_str(&format!("<{tag}>"));
                    tags.push(tag);
                }
            }
        }
        escape(&mut html, text);
        for tag in tags.into_iter().rev() {
            html.push_str(&format!("</{tag}>"));
        }
    }
    html.push_str("</div>");
    html
}

fn escape(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("<br>"),
            '\r' => {}
            _ => out.push(c),
        }
    }
}
