use std::{cell::RefCell, ops::Range};

use eframe::egui::{
    self, FontFamily, Key, Modifiers, ScrollArea, Stroke, TextBuffer, TextEdit, Ui,
    text::{CCursor, CCursorRange, LayoutJob},
    util::undoer::Undoer,
};

use super::{body::bold_family, theme::Theme};
use crate::{
    compose_format::{Span, Style, link_url, runs},
    model::Draft,
};

type Snapshot = (String, Vec<Span>, CCursorRange);

#[derive(Default)]
pub(super) struct Editor {
    pending: Option<Style>,
    cursor: Option<CCursorRange>,
    link: Option<LinkDialog>,
    history: Undoer<Snapshot>,
}

struct LinkDialog {
    range: Range<usize>,
    label: String,
    url: String,
    focus: bool,
    existing: bool,
}

enum LinkAction {
    Apply(String),
    Remove,
    Cancel,
}

struct Buffer<'a> {
    text: &'a mut String,
    spans: &'a RefCell<Vec<Span>>,
    pending: Option<&'a Style>,
}

impl Buffer<'_> {
    fn replace(&mut self, range: Range<usize>, text: &str, style: Style) -> usize {
        let end = self.byte_index_from_char_index(range.end);
        let start = self.byte_index_from_char_index(range.start);
        let count = text.chars().count();
        self.text.replace_range(start..end, text);

        let mut spans = self.spans.borrow_mut();
        let mut updated = Vec::new();
        for span in spans
            .iter()
            .take_while(|span| span.chars.start < range.start)
        {
            push_span(
                &mut updated,
                span.chars.start..span.chars.end.min(range.start),
                span.style.clone(),
            );
        }
        push_span(&mut updated, range.start..range.start + count, style);
        for span in spans.iter().filter(|span| span.chars.end > range.end) {
            let shift = |index| index - range.end + range.start + count;
            push_span(
                &mut updated,
                shift(span.chars.start.max(range.end))..shift(span.chars.end),
                span.style.clone(),
            );
        }
        *spans = updated;
        count
    }
}

fn push_span(spans: &mut Vec<Span>, chars: Range<usize>, style: Style) {
    if chars.is_empty() || style == Style::default() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.chars.end == chars.start
        && last.style == style
    {
        last.chars.end = chars.end;
    } else {
        spans.push(Span { chars, style });
    }
}

fn style_at(spans: &[Span], index: usize) -> Style {
    spans
        .iter()
        .find(|span| span.chars.contains(&index))
        .map_or_else(Style::default, |span| span.style.clone())
}

impl TextBuffer for Buffer<'_> {
    fn is_mutable(&self) -> bool {
        true
    }

    fn as_str(&self) -> &str {
        self.text
    }

    fn type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<Buffer<'static>>()
    }

    fn insert_text(&mut self, text: &str, index: usize) -> usize {
        let mut style = self.pending.cloned().unwrap_or_else(|| {
            let spans = self.spans.borrow();
            let mut style = index
                .checked_sub(1)
                .map_or_else(Style::default, |left| style_at(&spans, left));
            style.link = style_at(&spans, index)
                .link
                .filter(|url| style.link.as_ref() == Some(url));
            style
        });
        if text.contains('\n') {
            style.link = None;
        }
        self.replace(index..index, text, style)
    }

    fn delete_char_range(&mut self, range: Range<usize>) {
        let end = range.end.min(self.text.chars().count());
        self.replace(range.start.min(end)..end, "", Style::default());
    }
}

impl Editor {
    pub(super) fn editing_link(&self) -> bool {
        self.link.is_some()
    }

    pub(super) fn show(
        &mut self,
        ui: &mut Ui,
        draft: &mut Draft,
        theme: Theme,
        height: f32,
        reset: bool,
        focus: bool,
    ) -> bool {
        let id = ui.make_persistent_id("rich-message-body");
        if reset {
            *self = Self::default();
        }
        let focused = ui.memory(|memory| memory.has_focus(id));
        let consume = |mods, key| {
            focused && self.link.is_none() && ui.input_mut(|input| input.consume_key(mods, key))
        };
        let shortcuts =
            [Key::B, Key::I, Key::U, Key::K].map(|key| consume(Modifiers::COMMAND, key));
        let redo = consume(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z)
            || consume(Modifiers::COMMAND, Key::Y);
        let undo = consume(Modifiers::COMMAND, Key::Z);
        let time = ui.input(|input| input.time);
        let snapshot =
            |draft: &Draft, cursor| (draft.body.clone(), draft.formatting.clone(), cursor);
        let before = snapshot(draft, self.cursor.unwrap_or_default());
        let mut changed = false;
        if redo || undo {
            let restored = if redo {
                self.history.redo(&before)
            } else {
                self.history.undo(&before)
            };
            if let Some((body, spans, cursor)) = restored {
                self.cursor = Some(*cursor);
                draft.body.clone_from(body);
                draft.formatting.clone_from(spans);
                set_cursor(ui, id, *cursor);
                self.pending = None;
                changed = true;
            }
        }

        let spans = RefCell::new(std::mem::take(&mut draft.formatting));
        let bold = bold_family(ui.ctx());
        let output = ScrollArea::vertical()
            .id_salt("compose-body-scroll")
            .min_scrolled_height(height)
            .max_height(height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut layouter = |ui: &Ui, buffer: &dyn TextBuffer, width: f32| {
                    let mut job = LayoutJob::default();
                    job.wrap.max_width = width;
                    let mut base = egui::TextFormat::simple(
                        egui::TextStyle::Body.resolve(ui.style()),
                        theme.ink,
                    );
                    for (text, style) in runs(buffer.as_str(), &spans.borrow()) {
                        let mut format = base.clone();
                        if let Some(style) = style {
                            if style.bold {
                                format.font_id.family =
                                    bold.clone().unwrap_or(FontFamily::Proportional);
                                if bold.is_none() {
                                    format.font_id.size += 0.5;
                                }
                            }
                            if style.link.is_some() {
                                format.color = theme.accent;
                            }
                            if style.underline || style.link.is_some() {
                                format.underline = Stroke::new(1., format.color);
                            }
                            format.italics = style.italic;
                            if style.strike {
                                format.strikethrough = Stroke::new(1., format.color);
                            }
                        }
                        job.append(text, 0., format);
                    }
                    if job.sections.is_empty() {
                        base.color = ui.visuals().text_color();
                        job.append("", 0., base);
                    }
                    ui.fonts_mut(|fonts| fonts.layout_job(job))
                };
                TextEdit::multiline(&mut Buffer {
                    text: &mut draft.body,
                    spans: &spans,
                    pending: self.pending.as_ref(),
                })
                .id(id)
                .frame(false)
                .desired_width(f32::INFINITY)
                .min_size(egui::vec2(ui.available_width(), height))
                .hint_text("Write your message")
                .layouter(&mut layouter)
                .show(ui)
            })
            .inner;
        if focus {
            output.response.request_focus();
        }
        if output.cursor_range != self.cursor && !output.response.changed() {
            self.pending = None;
        }
        self.cursor = output.cursor_range.or(self.cursor);
        changed |= output.response.changed();
        draft.formatting = spans.into_inner();

        let range = self
            .cursor
            .map(|cursor| cursor.as_sorted_char_range())
            .unwrap_or_else(|| {
                let end = draft.body.chars().count();
                end..end
            });
        let mut action = None;
        ui.horizontal_wrapped(|ui| {
            for (index, (label, title, flag)) in [
                ("B", "Bold (Ctrl+B)", 0),
                ("I", "Italic (Ctrl+I)", 1),
                ("U", "Underline (Ctrl+U)", 2),
                ("S", "Strikethrough", 3),
            ]
            .into_iter()
            .enumerate()
            {
                let style = self.pending.clone().unwrap_or_else(|| {
                    style_at(
                        &draft.formatting,
                        if range.is_empty() {
                            range.start.saturating_sub(1)
                        } else {
                            range.start
                        },
                    )
                });
                let text = match flag {
                    0 => egui::RichText::new(label).strong(),
                    1 => egui::RichText::new(label).italics(),
                    2 => egui::RichText::new(label).underline(),
                    _ => egui::RichText::new(label).strikethrough(),
                };
                let clicked = ui
                    .selectable_label(flag_value(&style, flag), text)
                    .on_hover_text(title)
                    .clicked();
                if clicked || (index < 3 && shortcuts[index]) {
                    action = Some(flag);
                }
            }
            if ui
                .button("Link")
                .on_hover_text("Insert or edit link (Ctrl+K)")
                .clicked()
                || shortcuts[3]
            {
                self.link = Some(LinkDialog::new(draft, range.clone()));
            }
        });
        if let Some(flag) = action {
            if range.is_empty() {
                let mut style = self
                    .pending
                    .take()
                    .unwrap_or_else(|| style_at(&draft.formatting, range.start.saturating_sub(1)));
                style.link = None;
                let enabled = !flag_value(&style, flag);
                set_flag(&mut style, flag, enabled);
                self.pending = Some(style);
            } else {
                let spans = RefCell::new(std::mem::take(&mut draft.formatting));
                let mut buffer = Buffer {
                    text: &mut draft.body,
                    spans: &spans,
                    pending: None,
                };
                let enabled = !runs(
                    buffer.char_range(range.clone()),
                    &relative_spans(&spans.borrow(), &range),
                )
                .all(|(_, style)| style.is_some_and(|style| flag_value(style, flag)));
                let text = buffer.char_range(range.clone()).to_owned();
                let selected_spans = relative_spans(&spans.borrow(), &range);
                let mut index = range.start;
                for (text, style) in runs(&text, &selected_spans) {
                    let mut style = style.cloned().unwrap_or_default();
                    set_flag(&mut style, flag, enabled);
                    let count = text.chars().count();
                    buffer.replace(index..index + count, text, style);
                    index += count;
                }
                draft.formatting = spans.into_inner();
                changed = true;
            }
            set_cursor(
                ui,
                id,
                CCursorRange::two(CCursor::new(range.start), CCursor::new(range.end)),
            );
        }

        let mut committed = false;
        if let Some(link) = &mut self.link
            && let Some(action) = link.show(ui, id)
        {
            let link = self.link.take().unwrap();
            let cursor = match action {
                LinkAction::Apply(url) => {
                    committed = true;
                    link.apply(draft, Some(url))
                }
                LinkAction::Remove => {
                    committed = true;
                    link.apply(draft, None)
                }
                LinkAction::Cancel => {
                    CCursorRange::two(CCursor::new(link.range.start), CCursor::new(link.range.end))
                }
            };
            self.cursor = Some(cursor);
            self.pending = None;
            set_cursor(ui, id, cursor);
        }
        let after = snapshot(draft, self.cursor.unwrap_or_default());
        if (action.is_some() && changed) || committed {
            self.history.feed_state(time, &before);
            self.history.add_undo(&before);
            self.history.feed_state(time, &after);
            self.history.add_undo(&after);
        } else if !undo && !redo {
            self.history.feed_state(time, &after);
        }
        changed || committed
    }
}

fn relative_spans(spans: &[Span], range: &Range<usize>) -> Vec<Span> {
    spans
        .iter()
        .filter(|span| span.chars.end > range.start && span.chars.start < range.end)
        .map(|span| Span {
            chars: span.chars.start.saturating_sub(range.start)
                ..span.chars.end.min(range.end) - range.start,
            style: span.style.clone(),
        })
        .collect()
}

fn flag_value(style: &Style, flag: usize) -> bool {
    [style.bold, style.italic, style.underline, style.strike][flag]
}

fn set_flag(style: &mut Style, flag: usize, enabled: bool) {
    *[
        &mut style.bold,
        &mut style.italic,
        &mut style.underline,
        &mut style.strike,
    ][flag] = enabled;
}

fn set_cursor(ui: &Ui, id: egui::Id, cursor: CCursorRange) {
    let mut state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
    state.cursor.set_char_range(Some(cursor));
    state.store(ui.ctx(), id);
    ui.memory_mut(|memory| memory.request_focus(id));
}

impl LinkDialog {
    fn new(draft: &Draft, mut range: Range<usize>) -> Self {
        let mut url = String::new();
        let spans = &draft.formatting;
        let index = spans.iter().position(|span| {
            span.style.link.is_some()
                && (span.chars.contains(&range.start) || span.chars.end == range.start)
                && range.end <= span.chars.end
        });
        if let Some(index) = index {
            let span = &spans[index];
            url = span.style.link.clone().unwrap_or_default();
            let mut linked = span.chars.clone();
            for previous in spans[..index].iter().rev() {
                if previous.chars.end != linked.start
                    || previous.style.link.as_deref() != Some(&url)
                {
                    break;
                }
                linked.start = previous.chars.start;
            }
            for next in &spans[index + 1..] {
                if next.chars.start != linked.end || next.style.link.as_deref() != Some(&url) {
                    break;
                }
                linked.end = next.chars.end;
            }
            range = linked;
        }
        let label = draft
            .body
            .chars()
            .skip(range.start)
            .take(range.len())
            .collect();
        Self {
            range,
            label,
            url,
            focus: true,
            existing: index.is_some(),
        }
    }

    fn show(&mut self, ui: &Ui, id: egui::Id) -> Option<LinkAction> {
        let response = egui::Modal::new(id.with("link")).show(ui.ctx(), |ui| {
            ui.set_width(300.);
            ui.heading(if self.existing {
                "Edit link"
            } else {
                "Insert link"
            });
            ui.label("Text to display");
            let label = ui.text_edit_singleline(&mut self.label);
            ui.label("URL");
            let address = ui.text_edit_singleline(&mut self.url);
            if std::mem::take(&mut self.focus) {
                if self.label.is_empty() {
                    label.request_focus();
                } else {
                    address.request_focus();
                }
            }
            let valid = link_url(&self.url);
            let submit = ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter));
            let mut result = None;
            ui.horizontal(|ui| {
                if (ui
                    .add_enabled(valid.is_some(), egui::Button::new("Apply"))
                    .clicked()
                    || submit)
                    && let Some(url) = valid
                {
                    result = Some(LinkAction::Apply(url));
                }
                if self.existing && ui.button("Remove link").clicked() {
                    result = Some(LinkAction::Remove);
                }
                if ui.button("Cancel").clicked() {
                    result = Some(LinkAction::Cancel);
                }
            });
            result
        });
        let close = response.should_close();
        response.inner.or(close.then_some(LinkAction::Cancel))
    }

    fn apply(self, draft: &mut Draft, url: Option<String>) -> CCursorRange {
        let spans = RefCell::new(std::mem::take(&mut draft.formatting));
        let mut buffer = Buffer {
            text: &mut draft.body,
            spans: &spans,
            pending: None,
        };
        let original = buffer.char_range(self.range.clone()).to_owned();
        let label = if url.is_none() {
            original.as_str()
        } else if self.label.trim().is_empty() {
            url.as_deref().unwrap_or_default()
        } else {
            self.label.trim()
        };
        let mut end = self.range.start;
        if original == label {
            let selected = relative_spans(&spans.borrow(), &self.range);
            for (text, style) in runs(label, &selected) {
                let mut style = style.cloned().unwrap_or_default();
                style.link.clone_from(&url);
                let count = text.chars().count();
                buffer.replace(end..end + count, text, style);
                end += count;
            }
        } else {
            let mut style = style_at(&spans.borrow(), self.range.start);
            style.link.clone_from(&url);
            end += buffer.replace(self.range, label, style);
        }
        draft.formatting = spans.into_inner();
        CCursorRange::one(CCursor::new(end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_link_text_can_be_edited_and_keeps_neighboring_styles() {
        let mut draft = Draft {
            body: "é old tail".into(),
            formatting: vec![Span {
                chars: 2..10,
                style: Style {
                    bold: true,
                    ..Style::default()
                },
            }],
            ..Draft::default()
        };
        let mut link = LinkDialog::new(&draft, 2..5);
        link.label = "new label".into();
        let url = link_url("example.com").unwrap();
        link.apply(&mut draft, Some(url.clone()));
        assert_eq!(draft.body, "é new label tail");
        assert_eq!(draft.formatting[0].chars, 2..11);
        assert_eq!(draft.formatting[1].chars, 11..16);
        assert!(draft.formatting[1].style.bold);

        let link = LinkDialog::new(&draft, 4..4);
        assert_eq!(link.label, "new label");
        assert_eq!(link.url, url);
        link.apply(&mut draft, None);
        assert_eq!(draft.formatting.len(), 1);
        assert_eq!(draft.formatting[0].chars, 2..16);
        assert!(draft.formatting[0].style.link.is_none());
    }
}
