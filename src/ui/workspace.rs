use eframe::egui::{
    self, Align, Align2, Button, Color32, Context, FontId, Frame, Id, Label, Layout, Margin, Rect,
    RichText, ScrollArea, Sense, Stroke, TextEdit, Ui, pos2, vec2,
};

use super::{
    DownloadFiles, MailApp, Modal, PAGE, SaveLocation,
    body::bold_family,
    cached_files, name, subject, tag_name,
    theme::{Icon, icon, icon_button, paint_icon_button},
};
use crate::{
    model::{Action, Draft, Row, date_label},
    store::{Download, View, now},
};

impl MailApp {
    pub(super) fn header(&mut self, ctx: &Context) {
        let t = self.theme;
        egui::TopBottomPanel::top("top")
            .frame(
                Frame::new()
                    .fill(t.canvas)
                    .inner_margin(Margin::symmetric(14, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.set_min_height(42.);
                    let width = (ui.available_width() - 192.).clamp(210., 690.);
                    Frame::new()
                        .fill(t.raised)
                        .corner_radius(9)
                        .inner_margin(Margin::symmetric(8, 4))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let (rect, _) =
                                    ui.allocate_exact_size(vec2(28., 29.), Sense::hover());
                                icon(ui.painter(), Icon::Search, rect.center(), t.muted);
                                let response = ui.add(
                                    TextEdit::singleline(&mut self.query)
                                        .id(Id::new("global-search"))
                                        .hint_text("Search mail")
                                        .desired_width(width - 88.)
                                        .frame(false)
                                        .margin(Margin::symmetric(2, 5)),
                                );
                                if (response.has_focus() || response.lost_focus())
                                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                                {
                                    self.local_search(ctx);
                                }
                                if icon_button(ui, Icon::Tune, "Advanced search", t).clicked() {
                                    self.set_modal(Modal::Search);
                                }
                            });
                        });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        self.account_menu(ui);
                        if icon_button(ui, Icon::Settings, "Settings", t).clicked() {
                            self.set_modal(Modal::Settings);
                        }
                        if ui
                            .add(Button::new("?").frame(false).min_size(vec2(28., 30.)))
                            .on_hover_text("Keyboard shortcuts")
                            .clicked()
                        {
                            self.set_modal(Modal::Keys);
                        }
                    });
                });
            });
    }

    fn account_menu(&mut self, ui: &mut Ui) {
        let t = self.theme;
        let active = self.scope.as_ref().and_then(|id| self.account(id));
        let name = active.map_or("All accounts", |account| account.name.as_str());
        let color = active.map_or(t.ink, |account| t.account(&account.id).1);
        let mut change = None;
        egui::ComboBox::from_id_salt("mailbox-account")
            .width(118.)
            .wrap_mode(egui::TextWrapMode::Truncate)
            .selected_text(RichText::new(name).color(color))
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(self.scope.is_none(), "All accounts")
                    .clicked()
                {
                    change = Some(None);
                    ui.close();
                }
                ui.separator();
                for account in &self.accounts {
                    let (_, color) = t.account(&account.id);
                    let caption = RichText::new(format!("{}  ·  {}", account.name, account.email))
                        .color(color);
                    if ui
                        .selectable_label(self.scope.as_ref() == Some(&account.id), caption)
                        .clicked()
                    {
                        change = Some(Some(account.id.clone()));
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text("Switch account");
        if let Some(scope) = change {
            self.set_scope(scope);
        }
    }

    pub(super) fn sidebar(&mut self, ctx: &Context) {
        let t = self.theme;
        egui::SidePanel::left("sidebar")
            .exact_width(194.)
            .resizable(false)
            .frame(
                Frame::new()
                    .fill(t.canvas)
                    .inner_margin(Margin::symmetric(14, 5)),
            )
            .show(ctx, |ui| {
                ui.add_space(3.);
                let (rect, compose) = ui.allocate_exact_size(vec2(150., 46.), Sense::click());
                ui.painter().rect_filled(
                    rect,
                    10,
                    if compose.hovered() || compose.has_focus() {
                        t.primary_fill.lerp_to_gamma(Color32::WHITE, 0.15)
                    } else {
                        t.primary_fill
                    },
                );
                icon(
                    ui.painter(),
                    Icon::Pencil,
                    rect.left_center() + vec2(22., 0.),
                    t.on_primary,
                );
                ui.painter().text(
                    rect.left_center() + vec2(42., 0.),
                    Align2::LEFT_CENTER,
                    "Compose",
                    FontId::proportional(15.),
                    t.on_primary,
                );
                compose.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Compose")
                });
                if compose.on_hover_text("New message (c)").clicked() {
                    self.new_compose();
                }
                ui.add_space(12.);
                ScrollArea::vertical()
                    .id_salt("sidebar-scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 3.;
                        for (label, glyph, view) in [
                            ("Inbox", Icon::Inbox, View::Inbox),
                            ("Starred", Icon::Star, View::Tag("STARRED".into())),
                            ("Sent", Icon::Send, View::Tag("SENT".into())),
                            ("Drafts", Icon::Pencil, View::Drafts),
                            ("Outbox", Icon::Clock, View::Outbox),
                            ("Snoozed", Icon::Clock, View::Snoozed),
                            ("All mail", Icon::All, View::All),
                        ] {
                            if nav(ui, t, glyph, label, self.view == view) {
                                self.go(view);
                            }
                        }
                        ui.collapsing("More", |ui| {
                            for tag in [
                                "UNREAD",
                                "CATEGORY_PERSONAL",
                                "CATEGORY_UPDATES",
                                "CATEGORY_PROMOTIONS",
                                "CATEGORY_SOCIAL",
                                "CATEGORY_FORUMS",
                                "SPAM",
                                "TRASH",
                            ] {
                                let view = View::Tag(tag.into());
                                if nav(
                                    ui,
                                    t,
                                    if tag == "TRASH" {
                                        Icon::Trash
                                    } else {
                                        Icon::Tag
                                    },
                                    tag_name(tag),
                                    self.view == view,
                                ) {
                                    self.go(view);
                                }
                            }
                        });
                        self.label_sidebar(ui);
                        if !self.saved.is_empty() {
                            ui.add_space(10.);
                            ui.label(t.muted("Saved searches").size(12.).strong());
                            for search in self.saved.clone() {
                                let active = self.view == View::Search(search.query.clone())
                                    && self.scope == search.account;
                                let (rect, response) = nav_rect(ui, t, &search.name, active);
                                icon(
                                    ui.painter(),
                                    Icon::Search,
                                    rect.left_center() + vec2(16., 0.),
                                    t.muted,
                                );
                                response.context_menu(|ui| {
                                    if ui.button("Remove saved search").clicked() {
                                        if let Err(error) = self.store.hide_search(search.id) {
                                            self.toast(error.to_string(), true);
                                        }
                                        self.reload();
                                        ui.close();
                                    }
                                });
                                if response.on_hover_text(&search.query).clicked() {
                                    self.scope = search.account;
                                    self.query.clone_from(&search.query);
                                    self.set_view(View::Search(search.query));
                                }
                            }
                        }
                    });
            });
    }

    pub(super) fn archive_undo(&mut self, ctx: &Context) {
        const WINDOW: std::time::Duration = std::time::Duration::from_secs(10);
        let now = std::time::Instant::now();
        self.recent_archives
            .retain(|_, archived_at| now.duration_since(*archived_at) < WINDOW);
        let Some(oldest) = self.recent_archives.values().min() else {
            return;
        };
        ctx.request_repaint_after(WINDOW - now.duration_since(*oldest));

        let count = self.recent_archives.len();
        let caption = if count == 1 {
            "Thread archived".to_owned()
        } else {
            format!("{count} threads archived")
        };
        let mut undo = false;
        egui::Area::new(Id::new("archive_undo"))
            .movable(false)
            .anchor(Align2::RIGHT_BOTTOM, vec2(-16., -44.))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(caption);
                        undo = ui.button("Undo").clicked();
                    });
                });
            });
        if undo {
            let keys = self.recent_archives.keys().cloned().collect();
            if self.change(keys, Action::labels(&["INBOX"], &[])) {
                self.recent_archives.clear();
                self.toast("Archive undone", false);
            }
        }
    }

    pub(super) fn footer(&mut self, ctx: &Context) {
        let t = self.theme;
        egui::TopBottomPanel::bottom("bottom")
            .frame(
                Frame::new()
                    .fill(t.canvas)
                    .inner_margin(Margin::symmetric(16, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some((message, failed, _)) = &self.notify {
                        ui.label(RichText::new(message).size(12.).color(if *failed {
                            t.error
                        } else {
                            t.accent
                        }));
                    } else {
                        for a in self
                            .accounts
                            .iter()
                            .filter(|a| self.scope.as_ref().is_none_or(|id| id == &a.id))
                        {
                            if let Some(status) = self.statuses.get(&a.id) {
                                let color = if status.failed { t.error } else { t.accent };
                                let caption = if status.failed {
                                    "Offline / sync error"
                                } else {
                                    status.message.as_str()
                                };
                                ui.label(
                                    RichText::new(format!("{}  ·  {caption}", a.name))
                                        .size(12.)
                                        .color(color),
                                )
                                .on_hover_text(&status.message);
                                ui.add_space(12.);
                            }
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            t.muted(if self.demo {
                                "Sample mailbox"
                            } else {
                                "Local cache · 5 min sync"
                            })
                            .size(12.),
                        );
                        if let Some(id) = self.last_queued
                            && self.store.outbox().is_ok_and(|items| {
                                items
                                    .iter()
                                    .any(|item| item.id == id && item.status == "queued")
                            })
                            && ui.small_button("Undo send").clicked()
                        {
                            self.undo_send(id);
                        }
                    });
                });
            });
    }

    pub(super) fn workspace(&mut self, ctx: &Context) {
        let t = self.theme;
        egui::CentralPanel::default()
            .frame(Frame::new().fill(t.canvas).inner_margin(Margin {
                left: 5,
                right: 15,
                top: 2,
                bottom: 2,
            }))
            .show(ctx, |ui| {
                t.card(1).show(ui, |ui| {
                    if self.open.is_some() {
                        self.reader(ui, ctx);
                    } else {
                        self.mail_list(ui);
                    }
                });
            });
    }

    fn title(&self) -> String {
        match &self.view {
            View::Inbox => "Inbox".into(),
            View::All => "All mail".into(),
            View::Snoozed => "Snoozed".into(),
            View::Drafts => "Drafts".into(),
            View::Outbox => "Outbox".into(),
            View::Search(_) => "Search results".into(),
            View::Online => "Gmail search results".into(),
            View::Tag(id) => self
                .labels
                .iter()
                .find(|(account, label)| &label.id == id && self.scope.as_ref() == Some(account))
                .map(|(_, label)| label.name.clone())
                .unwrap_or_else(|| tag_name(id).to_owned()),
        }
    }

    fn mail_list(&mut self, ui: &mut Ui) {
        let t = self.theme;
        ui.set_min_size(ui.available_size());
        Frame::new()
            .inner_margin(Margin::symmetric(14, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(self.title()).size(19.).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if matches!(self.view, View::Search(_)) {
                            if ui.button("Search Gmail").clicked() {
                                self.gmail_search();
                            }
                            if ui
                                .button("Save")
                                .on_hover_text("Save this search")
                                .clicked()
                            {
                                self.set_modal(Modal::SaveSearch);
                            }
                        } else if matches!(self.view, View::Online)
                            && ui.button("Search offline").clicked()
                        {
                            self.local_search(ui.ctx());
                        }
                    });
                });
            });
        line(ui, t.line);
        if self.view != View::Outbox {
            self.list_toolbar(ui);
            line(ui, t.line);
        }
        if let Some(error) = self.errors.clone() {
            ui.add_space(18.);
            ui.vertical_centered(|ui| {
                ui.colored_label(t.error, &error);
                if matches!(self.view, View::Search(_)) && ui.button("Search Gmail").clicked() {
                    self.gmail_search();
                }
            });
            return;
        }
        if self.view == View::Outbox {
            self.outbox(ui);
            return;
        }
        if self.view == View::Online {
            if !self.online.pending.is_empty() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Searching Gmail…");
                });
            }
            for error in self.online.errors.values() {
                ui.colored_label(t.error, error);
            }
            if self.online.pending.is_empty()
                && !self.online.cursors.is_empty()
                && ui.button("Next Gmail results page").clicked()
            {
                self.grab_online_page();
            }
        }
        if self.rows.is_empty() && self.view != View::Drafts {
            ui.add_space(65.);
            ui.vertical_centered(|ui| {
                let text = match &self.view {
                    View::Inbox
                        if !self.demo
                            && self.statuses.values().any(|s| {
                                s.message.starts_with("Connecting")
                                    || s.message.starts_with("Caching")
                            }) =>
                    {
                        "Waiting for the first messages"
                    }
                    View::Inbox => "Inbox is empty",
                    View::Snoozed => "No snoozed threads",
                    View::Search(_) | View::Online => "No matching threads",
                    _ => "No threads here",
                };
                ui.label(RichText::new(text).size(19.).strong());
                if matches!(self.view, View::Search(_)) {
                    ui.label(t.muted("Search Gmail to include mail that hasn't synced yet."));
                }
            });
        }
        let rows = self.rows.iter().take(PAGE).cloned().collect::<Vec<_>>();
        let scroll_to = std::mem::take(&mut self.scroll_cursor);
        ScrollArea::vertical()
            .id_salt("mail-list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.view == View::Drafts {
                    self.draft_list(ui);
                }
                ui.spacing_mut().item_spacing.y = 0.;
                for (index, row) in rows.iter().enumerate() {
                    self.mail_row(ui, row, index, scroll_to && index == self.cursor);
                }
            });
    }

    fn list_toolbar(&mut self, ui: &mut Ui) {
        let t = self.theme;
        Frame::new()
            .inner_margin(Margin::symmetric(10, 3))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.;
                    let count = self.rows.len().min(PAGE);
                    let selected_here = self
                        .rows
                        .iter()
                        .take(PAGE)
                        .filter(|row| self.selected.contains(&row.key))
                        .count();
                    let checked = count > 0 && selected_here == count;
                    let (rect, pick) = ui.allocate_exact_size(vec2(25., 30.), Sense::click());
                    let state = if selected_here > 0 && !checked {
                        None
                    } else {
                        Some(checked)
                    };
                    checkbox(ui, rect, &pick, state, t);
                    if pick.on_hover_text("Select page (Ctrl+A)").clicked() && count > 0 {
                        if checked {
                            for row in self.rows.iter().take(PAGE) {
                                self.selected.remove(&row.key);
                            }
                        } else {
                            self.selected
                                .extend(self.rows.iter().take(PAGE).map(|row| row.key.clone()));
                        }
                    }
                    ui.add_enabled_ui(count > 0, |ui| {
                        egui::ComboBox::from_id_salt("bulk-select")
                            .width(24.)
                            .show_ui(ui, |ui| {
                                ui.set_min_width(143.);
                                type Selection = (&'static str, fn(&Row) -> bool);
                                let filters: [Selection; 5] = [
                                    ("All on page", |_| true),
                                    ("Unread on page", |row: &Row| row.unread),
                                    ("Read on page", |row: &Row| !row.unread),
                                    ("Starred on page", |row: &Row| row.starred),
                                    ("Unstarred on page", |row: &Row| !row.starred),
                                ];
                                for (caption, predicate) in filters {
                                    if ui.button(caption).clicked() {
                                        self.selected = self
                                            .rows
                                            .iter()
                                            .take(PAGE)
                                            .filter(|row| predicate(row))
                                            .map(|row| row.key.clone())
                                            .collect();
                                        ui.close();
                                    }
                                }
                                if ui.button("Clear selection").clicked() {
                                    self.selected.clear();
                                    ui.close();
                                }
                            })
                            .response
                            .on_hover_text("Select by status");
                    });
                    if self.selected.is_empty() {
                        if count > 0 {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .add_enabled_ui(self.rows.len() > PAGE, |ui| {
                                        icon_button(ui, Icon::Next, "Next page", t)
                                    })
                                    .inner
                                    .clicked()
                                {
                                    self.page(PAGE as isize);
                                }
                                if ui
                                    .add_enabled_ui(self.offset > 0, |ui| {
                                        icon_button(ui, Icon::Back, "Previous page", t)
                                    })
                                    .inner
                                    .clicked()
                                {
                                    self.page(-(PAGE as isize));
                                }
                                ui.label(
                                    t.muted(format!("{}–{}", self.offset + 1, self.offset + count))
                                        .size(12.),
                                );
                            });
                        }
                    } else {
                        self.actions(ui);
                        if ui
                            .small_button(format!("{} selected ×", self.selected.len()))
                            .on_hover_text("Clear selection")
                            .clicked()
                        {
                            self.selected.clear();
                        }
                    }
                });
            });
    }

    fn mail_row(&mut self, ui: &mut Ui, row: &Row, index: usize, scroll_to: bool) {
        let t = self.theme;
        let width = ui.available_width();
        let compact = width < 565.;
        let (rect, response) =
            ui.allocate_exact_size(vec2(width, if compact { 51. } else { 41. }), Sense::hover());
        let hovering = ui.rect_contains_pointer(rect);
        let hover_actions = hovering
            && !matches!(self.view, View::Drafts)
            && self.view != View::Tag("TRASH".into());
        let actions_width = 98.;
        let controls_end = if hover_actions {
            rect.right() - actions_width
        } else {
            rect.right()
        };
        let body = Rect::from_min_max(
            rect.left_top() + vec2(69., 0.),
            pos2(controls_end, rect.bottom()),
        );
        let row_id = ui.make_persistent_id((&row.key.account, &row.key.thread));
        let open = ui.interact(body, row_id.with("open"), Sense::click());
        let check = Rect::from_center_size(rect.left_center() + vec2(20., 0.), vec2(26., 28.));
        let check_response = ui.interact(check, row_id.with("check"), Sense::click());
        let star = Rect::from_center_size(rect.left_center() + vec2(46., 0.), vec2(26., 28.));
        let star_response = ui.interact(star, row_id.with("star"), Sense::click());
        let selected = self.selected.contains(&row.key);
        let opened = self.open.as_ref() == Some(&row.key);
        let p = ui.painter();
        let idle = if row.unread { t.unread } else { t.read };
        let background = if selected || opened {
            t.selected
        } else if hovering {
            idle.lerp_to_gamma(t.ink, 0.065)
        } else {
            idle
        };
        p.rect_filled(rect, 0, background);
        p.line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            Stroke::new(1., t.line),
        );
        if index == self.cursor || opened {
            p.rect_filled(
                Rect::from_min_size(rect.left_top(), vec2(2., rect.height())),
                0,
                t.accent,
            );
        }
        checkbox(ui, check, &check_response, Some(selected), t);
        let star_caption = if row.starred {
            "Remove star (s)"
        } else {
            "Star thread (s)"
        };
        paint_icon_button(ui, star, &star_response, Icon::Star, star_caption, t);
        if row.starred {
            icon(p, Icon::Star, star.center(), t.star);
            p.circle_filled(star.center(), 2.7, t.star);
        }
        if row.unread {
            let y = if compact {
                rect.top() + 14.5
            } else {
                rect.center().y
            };
            p.circle_filled(pos2(rect.left() + 62., y), 3., t.accent);
        }
        let identity = self
            .account(&row.key.account)
            .map_or("", |account| account.email.as_str());
        let (sender, participants) = crate::mime::conversation_senders(&row.senders, identity);
        let mut end = controls_end - 10.;
        let top = rect.top();
        if !hover_actions {
            end -= self.row_metadata(ui, row, rect, compact);
        }
        if compact {
            paint_senders(
                ui,
                Rect::from_min_max(pos2(body.left() + 3., top + 5.), pos2(end, top + 24.)),
                &sender,
                row,
                t,
            );
            paint_line(
                ui,
                Rect::from_min_max(
                    pos2(body.left() + 3., top + 25.),
                    pos2(end, rect.bottom() - 5.),
                ),
                subject(&row.subject),
                13.,
                t.mail_ink(row.unread),
                row.unread,
            );
        } else {
            let sender_width = (width * 0.165).clamp(118., 174.);
            let split = body.left() + sender_width;
            paint_senders(
                ui,
                Rect::from_min_max(
                    pos2(body.left() + 3., top),
                    pos2(split - 13., rect.bottom()),
                ),
                &sender,
                row,
                t,
            );
            let mut left = split;
            if width >= 785.
                && let Some((_, label)) = self.labels.iter().find(|(id, label)| {
                    id == &row.key.account && label.kind == "user" && row.labels.contains(&label.id)
                })
            {
                let (bg, fg) = t.account(&row.key.account);
                let badge = badge_width(ui, &label.name, 90.);
                paint_badge(ui, pos2(left, rect.center().y), badge, &label.name, bg, fg);
                left += badge + 8.;
            }
            paint_summary(
                ui,
                Rect::from_min_max(pos2(left, top), pos2(end, rect.bottom())),
                row,
                t,
            );
        }
        if hover_actions {
            self.row_actions(ui, row, rect);
        }
        if scroll_to {
            response.scroll_to_me(Some(Align::Center));
        }
        open.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                true,
                format!(
                    "{} · {sender} · {}",
                    if row.unread { "Unread" } else { "Read" },
                    subject(&row.subject)
                ),
            )
        });
        if open
            .on_hover_ui(|ui| {
                ui.label(RichText::new(subject(&row.subject)).strong());
                if !participants.is_empty() {
                    ui.label(t.muted(&participants));
                }
            })
            .clicked()
        {
            self.open_row(index);
        }
        if check_response.on_hover_text("Select thread (x)").clicked() {
            self.toggle_selected(row.key.clone());
        }
        if star_response.on_hover_text(star_caption).clicked() {
            let action = if row.starred {
                Action::labels(&[], &["STARRED"])
            } else {
                Action::labels(&["STARRED"], &[])
            };
            self.change(vec![row.key.clone()], action);
        }
    }

    fn row_metadata(&self, ui: &Ui, row: &Row, rect: Rect, compact: bool) -> f32 {
        let t = self.theme;
        let p = ui.painter();
        let right = rect.right() - 13.;
        let center = rect.center().y;
        p.text(
            pos2(right, if compact { rect.top() + 14. } else { center }),
            Align2::RIGHT_CENTER,
            date_label(row.date, false),
            line_font(ui, 11.5, row.unread),
            t.mail_ink(row.unread),
        );
        let mut reserved = 57.;
        if self.scope.is_none()
            && let Some(account) = self.account(&row.key.account)
        {
            let (bg, fg) = t.account(&account.id);
            let width = badge_width(ui, &account.name, if compact { 58. } else { 77. });
            let chip_right = if compact {
                right
            } else {
                right - reserved - 3.
            };
            paint_badge(
                ui,
                pos2(
                    chip_right - width,
                    if compact { rect.bottom() - 14. } else { center },
                ),
                width,
                &account.name,
                bg,
                fg,
            );
            if compact {
                reserved = reserved.max(width);
            } else {
                reserved += width + 8.;
            }
        }
        if row.attachments {
            icon(p, Icon::Clip, pos2(right - reserved - 9., center), t.muted);
            reserved += 22.;
        }
        reserved + 6.
    }

    fn row_actions(&mut self, ui: &mut Ui, row: &Row, rect: Rect) {
        let key = ui.make_persistent_id((&row.key.account, &row.key.thread, "hover-actions"));
        let (read_icon, read_caption, read) = if row.unread {
            (Icon::Read, "Mark read", Action::labels(&[], &["UNREAD"]))
        } else {
            (Icon::Mail, "Mark unread", Action::labels(&["UNREAD"], &[]))
        };
        for (i, (glyph, caption, action)) in [
            (Icon::Archive, "Archive", Action::labels(&[], &["INBOX"])),
            (read_icon, read_caption, read),
            (Icon::Trash, "Move to Trash", Action::Trash),
        ]
        .into_iter()
        .enumerate()
        {
            let rect = Rect::from_center_size(
                rect.right_center() - vec2(80. - 30. * i as f32, 0.),
                vec2(28., 29.),
            );
            let response = ui.interact(rect, key.with(i), Sense::click());
            paint_icon_button(ui, rect, &response, glyph, caption, self.theme);
            if response.on_hover_text(caption).clicked() {
                self.change(vec![row.key.clone()], action);
            }
        }
    }

    fn actions(&mut self, ui: &mut Ui) {
        if self.view == View::Tag("TRASH".into()) {
            if ui.button("Restore").clicked() {
                self.action(Action::Restore, true);
            }
            return;
        }
        let t = self.theme;
        if icon_button(ui, Icon::Archive, "Archive (e)", t).clicked() {
            self.action(Action::labels(&[], &["INBOX"]), true);
        }
        if icon_button(ui, Icon::Trash, "Move to Trash (#)", t).clicked() {
            self.action(Action::Trash, true);
        }
        ui.menu_button("More", |ui| {
            for (caption, action, close) in [
                ("Mark unread", Action::labels(&["UNREAD"], &[]), true),
                ("Mark read", Action::labels(&[], &["UNREAD"]), false),
                ("Move to Inbox", Action::labels(&["INBOX"], &[]), true),
                ("Report spam", Action::labels(&["SPAM"], &["INBOX"]), true),
                (
                    "Mark as not spam",
                    Action::labels(&["INBOX"], &["SPAM"]),
                    true,
                ),
            ] {
                if ui.button(caption).clicked() {
                    self.action(action, close);
                    ui.close();
                }
            }
        });
        ui.menu_button("Label", |ui| {
            let accounts: std::collections::HashSet<_> =
                self.targets().into_iter().map(|key| key.account).collect();
            let mut found = false;
            for (id, label) in self.labels.clone() {
                if label.kind != "user" || !accounts.contains(&id) {
                    continue;
                }
                found = true;
                let keys = self
                    .targets()
                    .into_iter()
                    .filter(|key| key.account == id)
                    .collect::<Vec<_>>();
                let applied = keys.iter().all(|key| {
                    self.rows
                        .iter()
                        .find(|row| &row.key == key)
                        .is_some_and(|row| row.labels.contains(&label.id))
                        || (self.open.as_ref() == Some(key)
                            && self
                                .messages
                                .iter()
                                .any(|item| item.labels.contains(&label.id)))
                });
                let account = self
                    .account(&id)
                    .map(|a| a.name.as_str())
                    .unwrap_or_default();
                if ui
                    .selectable_label(applied, format!("{} · {account}", label.name))
                    .clicked()
                {
                    let tags = [label.id.as_str()];
                    self.change(
                        keys,
                        if applied {
                            Action::labels(&[], &tags)
                        } else {
                            Action::labels(&tags, &[])
                        },
                    );
                    ui.close();
                }
            }
            if !found {
                ui.label("No labels for the selected accounts");
            }
        });
        ui.menu_button("Snooze", |ui| {
            for (label, ms) in [
                ("In one hour", 3_600_000),
                ("In one day", 86_400_000),
                ("In one week", 604_800_000),
            ] {
                if ui.button(label).clicked() {
                    self.snooze(now() + ms);
                    ui.close();
                }
            }
            if self.view == View::Snoozed && ui.button("Unsnooze").clicked() {
                self.snooze(now());
                ui.close();
            }
        });
    }

    fn reader(&mut self, ui: &mut Ui, ctx: &Context) {
        let Some(key) = self.open.clone() else {
            return;
        };
        let t = self.theme;
        let account = self
            .account(&key.account)
            .map(|a| a.email.clone())
            .unwrap_or_default();
        let title = self
            .messages
            .last()
            .map(|m| subject(&m.subject))
            .unwrap_or("Thread unavailable")
            .to_owned();
        let is_draft = self.view == View::Drafts;
        ui.set_min_size(ui.available_size());
        Frame::new()
            .inner_margin(Margin::symmetric(10, 3))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if icon_button(ui, Icon::Back, "Back to list (u or Esc)", t).clicked() {
                        self.open = None;
                    }
                    if !is_draft {
                        self.actions(ui);
                    }
                });
            });
        line(ui, t.line);
        let scroll = ScrollArea::vertical()
            .id_salt(("thread", &key.account, &key.thread))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                Frame::new()
                    .inner_margin(Margin {
                        left: 72,
                        right: 24,
                        top: 22,
                        bottom: 20,
                    })
                    .show(ui, |ui| {
                        ui.add(Label::new(RichText::new(title).size(25.)).wrap());
                        ui.label(t.muted(account).size(12.));
                    });
                let messages = self.messages.clone();
                for message in messages
                    .iter()
                    .filter(|m| is_draft || !m.labels.iter().any(|label| label == "DRAFT"))
                {
                    Frame::new()
                        .inner_margin(Margin::symmetric(24, 14))
                        .show(ui, |ui| {
                            let open = self.expanded.contains(&message.id);
                            let head = ui.horizontal(|ui| {
                                let (avatar, toggle) =
                                    ui.allocate_exact_size(vec2(40., 40.), Sense::click());
                                let (bg, fg) = t.account(&key.account);
                                ui.painter().circle_filled(avatar.center(), 19., bg);
                                let initials = name(&message.sender)
                                    .split_whitespace()
                                    .take(2)
                                    .filter_map(|part| part.chars().next())
                                    .flat_map(char::to_uppercase)
                                    .collect::<String>();
                                ui.painter().text(
                                    avatar.center(),
                                    Align2::CENTER_CENTER,
                                    initials,
                                    FontId::proportional(12.),
                                    fg,
                                );
                                let action = if open {
                                    "Collapse message"
                                } else {
                                    "Expand message"
                                };
                                toggle.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::WidgetType::Button,
                                        true,
                                        action,
                                    )
                                });
                                if toggle.on_hover_text(action).clicked() {
                                    if open {
                                        self.expanded.remove(&message.id);
                                    } else {
                                        self.expanded.insert(message.id.clone());
                                    }
                                }
                                ui.label(RichText::new(name(&message.sender)).strong())
                                    .on_hover_text(&message.sender);
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(t.muted(date_label(message.date, true)).size(12.));
                                });
                            });
                            head.response.on_hover_text(&message.sender);
                            if open {
                                Frame::new()
                                    .inner_margin(Margin {
                                        left: 48,
                                        right: 0,
                                        top: 0,
                                        bottom: 10,
                                    })
                                    .show(ui, |ui| {
                                        let mut parties = format!("To {}", message.recipients);
                                        if !message.cc.is_empty() {
                                            parties.push_str(&format!("  ·  Cc {}", message.cc));
                                        }
                                        if !message.bcc.is_empty() {
                                            parties.push_str(&format!("  ·  Bcc {}", message.bcc));
                                        }
                                        ui.label(t.muted(parties).size(12.));
                                        ui.add_space(13.);
                                        self.message_body(ui, &key, message);
                                        if !message.files.is_empty() {
                                            ui.add_space(14.);
                                            ui.separator();
                                            ui.label(RichText::new("Attachments").strong());
                                            self.attachment_previews(ui, &key, message);
                                        }
                                        ui.add_space(14.);
                                        ui.horizontal_wrapped(|ui| {
                                            for (index, file) in &message.files {
                                                let button = ui
                                                    .button(format!("↓  {file}"))
                                                    .on_hover_text("Choose a download folder");
                                                if button.clicked() {
                                                    self.save_file(
                                                        ctx,
                                                        &key,
                                                        &message.id,
                                                        Download::Attachment(*index),
                                                        file,
                                                        SaveLocation::ChooseFolder,
                                                    );
                                                }
                                                button.context_menu(|ui| {
                                                    if ui.button("Download to…").clicked() {
                                                        self.save_file(
                                                            ctx,
                                                            &key,
                                                            &message.id,
                                                            Download::Attachment(*index),
                                                            file,
                                                            SaveLocation::ChooseFolder,
                                                        );
                                                        ui.close();
                                                    }
                                                    if ui.button("Save to Downloads").clicked() {
                                                        self.save_file(
                                                            ctx,
                                                            &key,
                                                            &message.id,
                                                            Download::Attachment(*index),
                                                            file,
                                                            SaveLocation::Downloads,
                                                        );
                                                        ui.close();
                                                    }
                                                });
                                            }
                                            if message.files.len() > 1
                                                && ui.button("Download all").clicked()
                                            {
                                                self.save_files(
                                                    ctx,
                                                    &key,
                                                    &message.id,
                                                    DownloadFiles::Attachments(
                                                        message.files.clone(),
                                                    ),
                                                    SaveLocation::ChooseFolder,
                                                );
                                            }
                                            ui.menu_button("Message", |ui| {
                                                self.body_view_options(ui, &key, &message.id);
                                                if !is_draft {
                                                    if !self.demo
                                                        && ui.button("Draft with Luna").clicked()
                                                    {
                                                        self.start_luna(Some(message));
                                                        ui.close();
                                                    }
                                                    for (text, reply) in [
                                                        ("Reply", 0),
                                                        ("Reply all", 1),
                                                        ("Forward", 2),
                                                    ] {
                                                        if ui.button(text).clicked() {
                                                            self.reply_to(
                                                                Some(message.clone()),
                                                                reply,
                                                            );
                                                            ui.close();
                                                        }
                                                    }
                                                }
                                                if is_draft
                                                    && message
                                                        .labels
                                                        .iter()
                                                        .any(|tag| tag == "DRAFT")
                                                    && ui.button("Copy to local draft").clicked()
                                                {
                                                    self.open_compose(Draft {
                                                        account: key.account.clone(),
                                                        to: message.recipients.clone(),
                                                        cc: message.cc.clone(),
                                                        bcc: message.bcc.clone(),
                                                        subject: message.subject.clone(),
                                                        body: message.body.clone(),
                                                        files: cached_files(&key, message),
                                                        ..Draft::default()
                                                    });
                                                    ui.close();
                                                }
                                                if ui.button("Save original (.eml)").clicked() {
                                                    self.save_file(
                                                        ctx,
                                                        &key,
                                                        &message.id,
                                                        Download::Raw,
                                                        &format!("{}.eml", message.id),
                                                        SaveLocation::ChooseFolder,
                                                    );
                                                    ui.close();
                                                }
                                                if ui.button("Copy sender").clicked() {
                                                    ctx.copy_text(message.sender.clone());
                                                    ui.close();
                                                }
                                            });
                                        });
                                    });
                            }
                        });
                    line(ui, t.line);
                }
                if self.inline_compose() {
                    Frame::new()
                        .inner_margin(Margin {
                            left: 72,
                            right: 24,
                            top: 12,
                            bottom: 24,
                        })
                        .show(ui, |ui| {
                            let focus = self.draft_focus;
                            let frame = Frame::new()
                                .fill(t.surface)
                                .stroke(Stroke::new(1., t.line))
                                .corner_radius(14)
                                .inner_margin(Margin::same(18))
                                .show(ui, |ui| self.compose_editor(ui, ctx, true));
                            if focus {
                                frame.response.scroll_to_me(Some(Align::BOTTOM));
                            }
                        });
                } else if !is_draft && !messages.is_empty() {
                    Frame::new()
                        .inner_margin(Margin {
                            left: 72,
                            right: 24,
                            top: 16,
                            bottom: 24,
                        })
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                if t.primary(ui, "Reply").on_hover_text("Reply (r)").clicked() {
                                    self.reply_to(None, 0);
                                }
                                if ui
                                    .button("Reply all")
                                    .on_hover_text("Reply all (a)")
                                    .clicked()
                                {
                                    self.reply_to(None, 1);
                                }
                                if ui.button("Forward").on_hover_text("Forward (f)").clicked() {
                                    self.reply_to(None, 2);
                                }
                                if !self.demo
                                    && ui
                                        .button("Draft with Luna")
                                        .on_hover_text("Draft a reply with Luna (Shift+R)")
                                        .clicked()
                                {
                                    self.start_luna(None);
                                }
                            });
                        });
                }
            });
        #[cfg(target_os = "linux")]
        crate::web_reader::register_scroll_area(ctx, &scroll);
        #[cfg(not(target_os = "linux"))]
        let _ = scroll;
    }

    fn draft_list(&mut self, ui: &mut Ui) {
        let accounts = self.current_accounts();
        for draft in self
            .store
            .drafts()
            .unwrap_or_default()
            .into_iter()
            .filter(|d| accounts.contains(&d.account))
        {
            Frame::new()
                .inner_margin(Margin::symmetric(16, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button(subject(&draft.subject)).clicked() {
                            self.open_compose(draft.clone());
                        }
                        ui.label(self.theme.muted(&draft.to));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("Discard").clicked() {
                                if let Err(error) = self.store.discard_draft(draft.id) {
                                    self.toast(error.to_string(), true);
                                } else if self
                                    .draft
                                    .as_ref()
                                    .is_some_and(|open| open.id == draft.id)
                                {
                                    self.draft = None;
                                }
                            }
                        });
                    });
                });
        }
        if !self.rows.is_empty() {
            ui.add_space(12.);
            ui.label(self.theme.muted("Drafts in Gmail"));
        }
    }

    fn outbox(&mut self, ui: &mut Ui) {
        let accounts = self.current_accounts();
        match self.store.outbox() {
            Ok(outbox) => {
                let items = outbox
                    .into_iter()
                    .filter(|mail| accounts.contains(&mail.account))
                    .collect::<Vec<_>>();
                if items.is_empty() {
                    ui.add_space(65.);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("Outbox is empty").size(19.).strong());
                    });
                }
                for mail in items {
                    Frame::new()
                        .inner_margin(Margin::symmetric(17, 10))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(subject(&mail.subject)).strong());
                                    let status = match mail.status.as_str() {
                                        "queued" => {
                                            format!("Queued for {}", date_label(mail.due, true))
                                        }
                                        "sending" => "Sending…".into(),
                                        _ => mail.note,
                                    };
                                    ui.label(self.theme.muted(status).size(12.));
                                });
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if mail.status != "sending"
                                        && ui.button("Return to draft").clicked()
                                    {
                                        self.undo_send(mail.id);
                                    }
                                });
                            });
                        });
                    line(ui, self.theme.line);
                }
            }
            Err(error) => {
                ui.colored_label(self.theme.error, error.to_string());
            }
        }
    }
}

fn checkbox(ui: &Ui, rect: Rect, response: &egui::Response, state: Option<bool>, t: super::Theme) {
    let p = ui.painter();
    if response.hovered() || response.has_focus() {
        p.rect_filled(rect, 5, t.selected);
    }
    let center = rect.center();
    let outline = Rect::from_center_size(center, vec2(14., 14.));
    p.rect_stroke(
        outline,
        3,
        Stroke::new(
            1.25,
            if state == Some(false) {
                t.muted
            } else {
                t.accent
            },
        ),
        egui::StrokeKind::Inside,
    );
    let stroke = Stroke::new(1.55, t.accent);
    if state == Some(true) {
        p.add(egui::Shape::line(
            vec![
                center + vec2(-4., 0.),
                center + vec2(-1., 3.),
                center + vec2(4., -3.),
            ],
            stroke,
        ));
    } else if state.is_none() {
        p.line_segment([center - vec2(3.5, 0.), center + vec2(3.5, 0.)], stroke);
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Checkbox,
            response.enabled(),
            state == Some(true),
            "Select messages",
        )
    });
}

fn badge_width(ui: &Ui, label: &str, max: f32) -> f32 {
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_owned(), FontId::proportional(10.5), Color32::WHITE);
    (galley.size().x + 12.).min(max)
}

fn paint_badge(
    ui: &Ui,
    center_left: egui::Pos2,
    width: f32,
    text: &str,
    fill: Color32,
    color: Color32,
) {
    let rect = Rect::from_min_size(center_left - vec2(0., 8.5), vec2(width, 17.));
    ui.painter().rect_filled(rect, 4, fill);
    paint_line(ui, rect.shrink2(vec2(6., 0.)), text, 10.5, color, false);
}

fn paint_senders(ui: &Ui, rect: Rect, senders: &str, row: &Row, theme: super::Theme) {
    let sender_color = theme.mail_ink(row.unread);
    let mut text_rect = rect;
    if row.count > 1 {
        let count = row.count.to_string();
        let font = FontId::proportional(11.);
        let count_size = ui
            .painter()
            .layout_no_wrap(count.clone(), font.clone(), theme.muted)
            .size()
            .x;
        text_rect.max.x -= count_size + 6.;
        let sender_font = line_font(ui, 13., row.unread);
        let sender_width = ui
            .painter()
            .layout_no_wrap(senders.to_owned(), sender_font, sender_color)
            .size()
            .x;
        let right = (text_rect.left() + sender_width).min(text_rect.right());
        ui.painter().text(
            pos2(right + 6., rect.center().y),
            Align2::LEFT_CENTER,
            count,
            font,
            theme.muted,
        );
    }
    paint_line(ui, text_rect, senders, 13., sender_color, row.unread);
}

fn paint_summary(ui: &Ui, rect: Rect, row: &Row, t: super::Theme) {
    let mut job = egui::text::LayoutJob::default();
    let subject_ink = t.mail_ink(row.unread);
    job.append(
        subject(&row.subject),
        0.,
        egui::TextFormat {
            font_id: line_font(ui, 13.5, row.unread),
            color: subject_ink,
            ..Default::default()
        },
    );
    if !row.snippet.is_empty() {
        job.append(
            &format!("   ·   {}", row.snippet),
            0.,
            egui::TextFormat {
                font_id: FontId::proportional(13.),
                color: if row.unread { t.muted } else { t.read_preview },
                ..Default::default()
            },
        );
    }
    paint_job(ui, rect, job, subject_ink);
}

fn nav_rect(ui: &mut Ui, t: super::Theme, text: &str, active: bool) -> (Rect, egui::Response) {
    let (rect, res) = ui.allocate_exact_size(vec2(ui.available_width(), 29.), Sense::click());
    if active || res.hovered() || res.has_focus() {
        ui.painter()
            .rect_filled(rect, 6, if active { t.selected } else { t.raised });
    }
    if active {
        let marker = Rect::from_center_size(rect.left_center() + vec2(2., 0.), vec2(2., 16.));
        ui.painter().rect_filled(marker, 1, t.accent);
    }
    res.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, active, text)
    });
    paint_line(
        ui,
        Rect::from_min_max(
            rect.left_top() + vec2(32., 5.),
            rect.right_bottom() - vec2(7., 4.),
        ),
        text,
        13.,
        if active { t.accent } else { t.ink },
        active,
    );
    (rect, res)
}

fn nav(ui: &mut Ui, t: super::Theme, glyph: Icon, text: &str, active: bool) -> bool {
    let (rect, response) = nav_rect(ui, t, text, active);
    icon(
        ui.painter(),
        glyph,
        rect.left_center() + vec2(16., 0.),
        if active { t.accent } else { t.muted },
    );
    response.clicked()
}

pub(super) fn paint_line(ui: &Ui, rect: Rect, text: &str, size: f32, color: Color32, strong: bool) {
    if rect.width() < 12. {
        return;
    }
    let job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id: line_font(ui, size, strong),
            color,
            ..Default::default()
        },
    );
    paint_job(ui, rect, job, color);
}

fn line_font(ui: &Ui, size: f32, strong: bool) -> FontId {
    if strong {
        return bold_family(ui.ctx()).map_or_else(
            || FontId::proportional(size + 0.45),
            |family| FontId::new(size, family),
        );
    }
    FontId::proportional(size)
}

fn paint_job(ui: &Ui, rect: Rect, mut job: egui::text::LayoutJob, color: Color32) {
    if rect.width() < 12. {
        return;
    }
    job.wrap.max_width = rect.width();
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    ui.painter().with_clip_rect(rect).galley(
        rect.left_center() - vec2(0., galley.size().y / 2.),
        galley,
        color,
    );
}

fn line(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.), Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.top(), Stroke::new(1., color));
}
