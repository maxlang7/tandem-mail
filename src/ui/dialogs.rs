use chrono::{Local, NaiveDateTime, TimeZone};
use eframe::egui::{
    self, Align, Align2, Context, Grid, Layout, RichText, ScrollArea, TextEdit, Ui, vec2,
};

use super::{MailApp, Modal, subject};
use crate::{
    model::{Account, Criteria, Filter, date_label},
    search,
    store::{View, now},
    sync::Command,
};

impl MailApp {
    pub(super) fn inline_compose(&self) -> bool {
        self.draft
            .as_ref()
            .zip(self.open.as_ref())
            .is_some_and(|(draft, key)| {
                draft.account == key.account && draft.thread.as_ref() == Some(&key.thread)
            })
    }

    pub(super) fn compose(&mut self, ctx: &Context) {
        if self.draft.is_none() || self.inline_compose() {
            return;
        }
        let rect = ctx.content_rect();
        let width = (rect.width() * 0.56)
            .clamp(420., 780.)
            .min(rect.width() - 24.);
        let height = (rect.height() * 0.78)
            .clamp(420., 720.)
            .min(rect.height() - 32.);
        let mut visible = true;
        egui::Window::new("New message")
            .id(egui::Id::new("composer-gmail"))
            .collapsible(false)
            .default_size([width, height])
            .default_pos(rect.right_bottom() - vec2(width + 24., height + 32.))
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(self.theme.surface)
                    .inner_margin(egui::Margin::same(16))
                    .corner_radius(12),
            )
            .open(&mut visible)
            .show(ctx, |ui| self.compose_editor(ui, ctx, false));
        if !visible {
            self.draft = None;
        }
    }

    pub(super) fn compose_editor(&mut self, ui: &mut Ui, _ctx: &Context, inline: bool) {
        let Some(mut draft) = self.draft.take() else {
            return;
        };
        let t = self.theme;
        let mut visible = true;
        let mut changed = false;
        let mut close = false;
        let mut discard = false;
        let mut queue = false;
        let mut schedule = false;
        let focus = std::mem::take(&mut self.draft_focus);
        let extra_id = ui.make_persistent_id("extra-recipients");
        let mut extra = ui.data(|data| data.get_temp::<bool>(extra_id).unwrap_or(false))
            || !draft.cc.is_empty()
            || !draft.bcc.is_empty();
        ui.horizontal(|ui| {
            ui.label(t.muted(if inline { "Reply" } else { "Message" }));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .small_button(if extra { "Hide Cc / Bcc" } else { "Cc / Bcc" })
                    .clicked()
                {
                    extra = !extra;
                    ui.data_mut(|data| data.insert_temp(extra_id, extra));
                }
            });
        });
        Grid::new("compose-fields")
            .num_columns(2)
            .spacing([12., 6.])
            .show(ui, |ui| {
                ui.label(t.muted("From"));
                if draft.thread.is_some() {
                    ui.label(
                        self.account(&draft.account)
                            .map(|a| a.email.as_str())
                            .unwrap_or(""),
                    );
                } else {
                    let previous = draft.account.clone();
                    account_selector(ui, "draft-account", &mut draft.account, &self.accounts);
                    changed |= previous != draft.account;
                }
                ui.end_row();
                let account = self
                    .accounts
                    .iter()
                    .find(|account| account.id == draft.account);
                let focus_to = focus && draft.to.is_empty();
                for (label, value, focus_field) in [
                    ("To", &mut draft.to, focus_to),
                    ("Cc", &mut draft.cc, false),
                    ("Bcc", &mut draft.bcc, false),
                ] {
                    if label != "To" && !extra {
                        continue;
                    }
                    ui.label(t.muted(label));
                    changed |=
                        self.recipients
                            .field(ui, label, value, account, &self.store, focus_field);
                    ui.end_row();
                }
                ui.label(t.muted("Subject"));
                changed |= ui
                    .add(
                        TextEdit::singleline(&mut draft.subject)
                            .frame(false)
                            .desired_width(f32::INFINITY),
                    )
                    .changed();
                ui.end_row();
            });
        ui.separator();
        ui.add_space(8.);
        let body_height = if inline {
            260.
        } else {
            (ui.available_height() - 114.).max(160.)
        };
        let focus_body = focus && !draft.to.is_empty();
        changed |= self
            .composer
            .show(ui, &mut draft, t, body_height, focus, focus_body);
        if !draft.files.is_empty() {
            let mut remove = None;
            ui.horizontal_wrapped(|ui| {
                for (i, file) in draft.files.iter().enumerate() {
                    let name = &file.name;
                    if ui
                        .button(format!("{name}  ×"))
                        .on_hover_text("Remove attachment")
                        .clicked()
                    {
                        remove = Some(i);
                    }
                }
            });
            if let Some(index) = remove {
                draft.files.remove(index);
                changed = true;
            }
        }
        ui.horizontal_wrapped(|ui| {
            if t.primary(ui, "Send")
                .on_hover_text("Send with a 10-second undo window (Ctrl+Enter)")
                .clicked()
            {
                queue = true;
            }
            if ui.button("Schedule").clicked() {
                schedule = true;
            }
            if ui.button("Attach").clicked()
                && let Some(files) = rfd::FileDialog::new().pick_files()
            {
                for file in files {
                    match self.store.stage_file(&file) {
                        Ok(file) => {
                            draft.files.push(file);
                            changed = true;
                        }
                        Err(error) => self.toast(error.to_string(), true),
                    }
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Discard").clicked() {
                    discard = true;
                }
                if ui
                    .button("Close")
                    .on_hover_text("Keep the draft locally")
                    .clicked()
                {
                    close = true;
                }
                if draft.id != 0 {
                    ui.label(t.muted("Local draft").size(12.));
                }
            });
        });
        if discard {
            if let Err(error) = self.store.discard_draft(draft.id) {
                self.toast(error.to_string(), true);
                self.draft = Some(draft);
            }
            return;
        }
        if close {
            visible = false;
        }
        if changed && let Err(error) = self.store.save_draft(&mut draft) {
            self.toast(format!("Could not save draft: {error}"), true);
            visible = true;
        }
        if visible {
            self.draft = Some(draft);
        }
        if queue {
            self.queue(now() + 10_000);
        }
        if schedule {
            self.set_modal(Modal::Schedule);
            self.modal_value = (Local::now() + chrono::Duration::hours(1))
                .format("%Y-%m-%d %H:%M")
                .to_string();
        }
    }

    pub(super) fn queue(&mut self, at: i64) {
        if self.demo {
            self.toast("Sending is disabled in the sample mailbox", false);
            return;
        }
        let Some(mut draft) = self.draft.take() else {
            return;
        };
        let Some(account) = self.account(&draft.account).cloned() else {
            self.toast("Select the sender account", true);
            self.draft = Some(draft);
            return;
        };
        match self.store.queue(&mut draft, &account, at) {
            Ok(id) => {
                self.last_queued = Some(id);
                self.toast(format!("Queued for {}", date_label(at, true)), false);
            }
            Err(error) => {
                self.toast(error.to_string(), true);
                self.draft = Some(draft);
            }
        }
    }

    pub(super) fn dialogs(&mut self, ctx: &Context) {
        let Some(modal) = self.modal else {
            return;
        };
        let mut show = true;
        let title = match modal {
            Modal::Search => "Advanced search",
            Modal::Filters => "Gmail filters",
            Modal::Settings => "Settings",
            Modal::Keys => "Keyboard shortcuts",
            Modal::SaveSearch => "Save search",
            Modal::CreateLabel => "Create label",
            Modal::Schedule => "Schedule send",
            Modal::Palette => "Go to",
            Modal::Luna => "Draft with Luna",
        };
        egui::Window::new(title)
            .id(egui::Id::new(("dialog", title)))
            .open(&mut show)
            .anchor(Align2::CENTER_CENTER, [0., -25.])
            .collapsible(false)
            .resizable(false)
            .default_width(if modal == Modal::Luna {
                510.
            } else if matches!(modal, Modal::Keys | Modal::Filters) {
                475.
            } else {
                410.
            })
            .show(ctx, |ui| match modal {
                Modal::Search => self.advanced(ui),
                Modal::Filters => self.filters(ui),
                Modal::Settings => self.settings(ui),
                Modal::Keys => shortcuts(ui),
                Modal::Palette => self.palette(ui),
                Modal::SaveSearch => self.save_search(ui),
                Modal::CreateLabel => self.new_label(ui),
                Modal::Schedule => self.schedule(ui),
                Modal::Luna => self.luna_dialog(ui),
            });
        if !show && self.modal == Some(modal) {
            self.modal = None;
            if modal == Modal::Luna {
                self.luna = None;
            }
        }
    }

    fn advanced(&mut self, ui: &mut Ui) {
        let fields = &mut self.search_form;
        Grid::new("search-grid")
            .num_columns(2)
            .spacing([12., 8.])
            .show(ui, |ui| {
                for (caption, value, hint) in [
                    ("From", &mut fields.from, "Sender address or name"),
                    ("To", &mut fields.to, "Recipient address or name"),
                    ("Subject", &mut fields.subject, "Subject phrase"),
                    ("Words", &mut fields.words, "Text or search operators"),
                    ("Exclude words", &mut fields.excludes, "Phrase"),
                    ("After", &mut fields.after, "YYYY-MM-DD"),
                    ("Before", &mut fields.before, "YYYY-MM-DD"),
                    ("Filename", &mut fields.filename, "Filename or extension"),
                    ("Larger than", &mut fields.size, "Example 5M"),
                ] {
                    field(ui, caption, value, hint);
                }
            });
        ui.horizontal(|ui| {
            ui.checkbox(&mut fields.attachment, "Has attachment");
            ui.checkbox(&mut fields.unread, "Unread");
            ui.checkbox(&mut fields.starred, "Starred");
        });
        ui.add_space(7.);
        let mut target = None;
        ui.horizontal(|ui| {
            if self.theme.primary(ui, "Search offline").clicked() {
                target = Some(false);
            }
            if ui.button("Search Gmail").clicked() {
                target = Some(true);
            }
            if ui.button("Clear").clicked() {
                self.search_form = Default::default();
            }
        });
        if let Some(online) = target {
            let fields = &self.search_form;
            let mut terms = vec![fields.words.trim().to_owned()];
            for (operator, value) in [
                ("from:", &fields.from),
                ("to:", &fields.to),
                ("subject:", &fields.subject),
                ("-", &fields.excludes),
                ("after:", &fields.after),
                ("before:", &fields.before),
                ("filename:", &fields.filename),
                ("larger:", &fields.size),
            ] {
                if !value.trim().is_empty() {
                    terms.push(format!("{operator}{}", search::quoted(value.trim())));
                }
            }
            for (enabled, term) in [
                (fields.attachment, "has:attachment"),
                (fields.starred, "is:starred"),
                (fields.unread, "is:unread"),
            ] {
                if enabled {
                    terms.push(term.into());
                }
            }
            self.query = terms.join(" ").trim().to_owned();
            self.modal = None;
            if online {
                self.gmail_search();
            } else {
                self.local_search(ui.ctx());
            }
        }
    }

    fn filters(&mut self, ui: &mut Ui) {
        let previous = self.modal_account.clone();
        account_selector(
            ui,
            "filters-account",
            &mut self.modal_account,
            &self.accounts,
        );
        if previous != self.modal_account {
            self.confirm_filter = None;
            self.filter
                .action
                .add_label_ids
                .retain(|label| !label.starts_with("Label_"));
            self.filter.action.forward.clear();
            self.filter_view = self.store.filters(&self.modal_account).unwrap_or_default();
            self.command(&self.modal_account.clone(), Command::ReadFilters);
        }
        ui.add_space(6.);
        ui.label(RichText::new("Existing rules").strong());
        if self.filter_view.is_empty() {
            ui.label(self.theme.muted("No rules loaded"));
        }
        ScrollArea::vertical()
            .max_height(150.)
            .id_salt("filter-list")
            .show(ui, |ui| {
                for filter in self.filter_view.clone() {
                    ui.group(|ui| {
                        let criteria = &filter.criteria;
                        let mut conditions = [
                            ("From", &criteria.from),
                            ("To", &criteria.to),
                            ("Subject", &criteria.subject),
                            ("Words", &criteria.query),
                            ("Excludes", &criteria.negated_query),
                        ]
                        .into_iter()
                        .filter(|(_, v)| !v.is_empty())
                        .map(|(label, value)| format!("{label} {value}"))
                        .collect::<Vec<_>>();
                        if criteria.has_attachment {
                            conditions.push("Has attachment".into());
                        }
                        if let Some(size) = criteria.size {
                            conditions.push(format!(
                                "Size {} {size} B",
                                criteria.size_comparison.as_deref().unwrap_or("")
                            ));
                        }
                        let mut actions = vec![];
                        for (name, tags) in [
                            ("Add", &filter.action.add_label_ids),
                            ("Remove", &filter.action.remove_label_ids),
                        ] {
                            for tag in tags.iter().filter(|tag| tag.as_str() != "IMPORTANT") {
                                let label = self
                                    .labels
                                    .iter()
                                    .find(|(account, label)| {
                                        account == &self.modal_account && &label.id == tag
                                    })
                                    .map_or(tag.as_str(), |(_, label)| label.name.as_str());
                                actions.push(format!("{name} {label}"));
                            }
                        }
                        if !filter.action.forward.is_empty() {
                            actions.push(format!("Forward to {}", filter.action.forward));
                        }
                        let mut summary = conditions.join(" · ");
                        if !actions.is_empty() {
                            summary.push_str(" → ");
                            summary.push_str(&actions.join(" · "));
                        }
                        ui.label(summary).on_hover_text(&filter.id);
                        ui.horizontal(|ui| {
                            if self.confirm_filter.as_deref() == Some(filter.id.as_str()) {
                                if ui.button("Delete this Gmail rule").clicked() {
                                    self.command(
                                        &self.modal_account.clone(),
                                        Command::DeleteFilter(filter.id.clone()),
                                    );
                                    self.confirm_filter = None;
                                }
                                if ui.button("Cancel").clicked() {
                                    self.confirm_filter = None;
                                }
                            } else {
                                if ui.small_button("Duplicate").clicked() {
                                    self.filter = filter.clone();
                                    self.filter
                                        .action
                                        .add_label_ids
                                        .retain(|tag| tag != "IMPORTANT");
                                    self.filter
                                        .action
                                        .remove_label_ids
                                        .retain(|tag| tag != "IMPORTANT");
                                    self.filter.id.clear();
                                    self.filter_size = criteria
                                        .size
                                        .map(|size| size.to_string())
                                        .unwrap_or_default();
                                }
                                if ui.small_button("Delete…").clicked() {
                                    self.confirm_filter = Some(filter.id.clone());
                                }
                            }
                        });
                    });
                }
            });
        ui.separator();
        ui.label(RichText::new("Create a rule").strong());
        let c = &mut self.filter.criteria;
        Grid::new("filter-grid")
            .num_columns(2)
            .spacing([12., 6.])
            .show(ui, |ui| {
                for (label, value, hint) in [
                    ("From", &mut c.from, "Sender"),
                    ("To", &mut c.to, "Recipient"),
                    ("Subject", &mut c.subject, "Subject phrase"),
                    ("Has words", &mut c.query, "Gmail query"),
                    ("Doesn't have", &mut c.negated_query, "Excluded words"),
                ] {
                    field(ui, label, value, hint);
                }
                ui.label("Size");
                ui.horizontal(|ui| {
                    ui.add(
                        TextEdit::singleline(&mut self.filter_size)
                            .desired_width(118.)
                            .hint_text("Bytes"),
                    );
                    egui::ComboBox::from_id_salt("size-comp")
                        .selected_text(match c.size_comparison.as_deref() {
                            Some("smaller") => "Smaller than",
                            _ => "Larger than",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut c.size_comparison,
                                Some("larger".into()),
                                "Larger than",
                            );
                            ui.selectable_value(
                                &mut c.size_comparison,
                                Some("smaller".into()),
                                "Smaller than",
                            );
                        });
                });
                ui.end_row();
            });
        ui.checkbox(&mut c.has_attachment, "Has an attachment");
        ui.add_space(7.);
        let action = &mut self.filter.action;
        ui.horizontal_wrapped(|ui| {
            flag(ui, "Skip inbox", &mut action.remove_label_ids, "INBOX");
            flag(ui, "Mark read", &mut action.remove_label_ids, "UNREAD");
            flag(ui, "Star", &mut action.add_label_ids, "STARRED");
            flag(ui, "Move to Trash", &mut action.add_label_ids, "TRASH");
            flag(
                ui,
                "Never send to spam",
                &mut action.remove_label_ids,
                "SPAM",
            );
        });
        ui.menu_button("Apply label or category", |ui| {
            for (_, label) in self.labels.iter().filter(|(acct, label)| {
                acct == &self.modal_account
                    && (label.kind == "user" || label.id.starts_with("CATEGORY_"))
            }) {
                flag(
                    ui,
                    &label.name,
                    &mut self.filter.action.add_label_ids,
                    &label.id,
                );
            }
        });
        ui.horizontal(|ui| {
            ui.label("Forward to");
            ui.add(
                TextEdit::singleline(&mut self.filter.action.forward)
                    .hint_text("Verified Gmail forwarding address")
                    .desired_width(270.),
            );
        });
        ui.add_space(7.);
        if self.theme.primary(ui, "Create Gmail filter").clicked() {
            match self.filter_request() {
                Ok(filter) => {
                    self.command(&self.modal_account.clone(), Command::CreateFilter(filter));
                }
                Err(error) => self.toast(error.to_string(), true),
            }
        }
    }

    fn filter_request(&self) -> anyhow::Result<Filter> {
        let criteria = &self.filter.criteria;
        let size = if self.filter_size.trim().is_empty() {
            None
        } else {
            Some(
                self.filter_size
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| anyhow::anyhow!("Use a number of bytes"))?,
            )
        };
        let criteria = Criteria {
            from: criteria.from.trim().into(),
            to: criteria.to.trim().into(),
            subject: criteria.subject.trim().into(),
            query: criteria.query.trim().into(),
            negated_query: criteria.negated_query.trim().into(),
            has_attachment: criteria.has_attachment,
            size,
            size_comparison: size.map(|_| {
                criteria
                    .size_comparison
                    .clone()
                    .unwrap_or_else(|| "larger".into())
            }),
        };
        let mut action = self.filter.action.clone();
        action.forward = action.forward.trim().to_owned();
        if !action.forward.is_empty() {
            anyhow::ensure!(
                action.forward.parse::<lettre::Address>().is_ok(),
                "Enter a forwarding email address"
            );
        }
        anyhow::ensure!(
            criteria.has_attachment
                || criteria.size.is_some()
                || [
                    &criteria.from,
                    &criteria.to,
                    &criteria.subject,
                    &criteria.query,
                    &criteria.negated_query
                ]
                .iter()
                .any(|v| !v.is_empty()),
            "Add at least one filter criterion"
        );
        anyhow::ensure!(
            !action.add_label_ids.is_empty()
                || !action.remove_label_ids.is_empty()
                || !action.forward.is_empty(),
            "Choose at least one action"
        );
        anyhow::ensure!(
            action
                .add_label_ids
                .iter()
                .filter(|id| id.starts_with("Label_"))
                .count()
                <= 1,
            "Gmail permits one custom label per rule"
        );
        Ok(Filter {
            id: String::new(),
            criteria,
            action,
        })
    }

    fn settings(&mut self, ui: &mut Ui) {
        let mut dark = self.theme.dark;
        ui.horizontal(|ui| {
            ui.label("Appearance");
            ui.selectable_value(&mut dark, true, "Gruvbox dark");
            ui.selectable_value(&mut dark, false, "Gruvbox light");
        });
        if dark != self.theme.dark {
            if let Err(error) = self.store.set_dark(dark) {
                self.toast(error.to_string(), true);
            }
            self.theme = super::Theme::new(dark);
            self.theme.apply(ui.ctx());
        }
        ui.add_space(7.);
        if ui.button("Gmail filters").clicked() {
            self.set_modal(Modal::Filters);
        }
        ui.add_space(7.);
        ui.separator();
        for account in &self.accounts {
            ui.label(RichText::new(&account.email).strong());
            if let Some(status) = self.statuses.get(&account.id) {
                ui.colored_label(
                    if status.failed {
                        self.theme.error
                    } else {
                        self.theme.muted
                    },
                    &status.message,
                );
                if let Some(last) = status.last_sync {
                    ui.label(
                        self.theme
                            .muted(format!("Last checked {}", date_label(last, true)))
                            .size(12.),
                    );
                }
            }
            ui.add_space(5.);
        }
        if !self.demo {
            ui.separator();
            ui.label(self.theme.muted("Cache location").size(12.));
            ui.add(egui::Label::new(self.path.display().to_string()).selectable(true));
        }
    }

    fn palette(&mut self, ui: &mut Ui) {
        let answer = ui.add(
            TextEdit::singleline(&mut self.modal_value)
                .desired_width(f32::INFINITY)
                .hint_text("Find a command"),
        );
        if std::mem::take(&mut self.modal_focus) {
            answer.request_focus();
        }
        let query = self.modal_value.to_lowercase();
        let choices = [
            ("Compose", "c", 0),
            ("Inbox", "g i", 1),
            ("NMRs", "g n", 11),
            ("Starred", "g s", 2),
            ("Sent", "g t", 3),
            ("Drafts", "g d", 4),
            ("All mail", "g a", 5),
            ("Advanced search", "", 6),
            ("Gmail filters", "", 7),
            ("Refresh mail", "Ctrl+R", 8),
            ("Keyboard shortcuts", "?", 9),
            ("Settings", "", 10),
        ];
        let matches = choices
            .into_iter()
            .filter(|(text, _, _)| text.to_lowercase().contains(&query))
            .collect::<Vec<_>>();
        let mut action = if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            matches.first().map(|row| row.2)
        } else {
            None
        };
        for (text, shortcut, code) in matches {
            ui.horizontal(|ui| {
                if ui.button(text).clicked() {
                    action = Some(code);
                }
                if !shortcut.is_empty() {
                    ui.label(self.theme.muted(shortcut).monospace());
                }
            });
        }
        if let Some(action) = action {
            self.modal = None;
            match action {
                0 => self.new_compose(),
                1 => self.go(View::Inbox),
                2 => self.go(View::Tag("STARRED".into())),
                3 => self.go(View::Tag("SENT".into())),
                4 => self.go(View::Drafts),
                5 => self.go(View::All),
                6 => self.set_modal(Modal::Search),
                7 => self.set_modal(Modal::Filters),
                8 => self.refresh(),
                9 => self.set_modal(Modal::Keys),
                10 => self.set_modal(Modal::Settings),
                11 => self.go_to_nmrs(),
                _ => {}
            }
        }
    }

    fn save_search(&mut self, ui: &mut Ui) {
        let View::Search(query) = &self.view else {
            return;
        };
        ui.label(self.theme.muted(query));
        ui.add(
            TextEdit::singleline(&mut self.modal_value)
                .desired_width(f32::INFINITY)
                .hint_text("Name"),
        );
        let value = self.modal_value.trim();
        if ui
            .add_enabled(!value.is_empty(), egui::Button::new("Save search"))
            .clicked()
        {
            match self.store.save_search(value, query, self.scope.as_deref()) {
                Ok(()) => {
                    self.modal = None;
                    self.reload();
                }
                Err(error) => self.toast(error.to_string(), true),
            }
        }
    }

    fn new_label(&mut self, ui: &mut Ui) {
        account_selector(
            ui,
            "new-label-account",
            &mut self.modal_account,
            &self.accounts,
        );
        ui.add(
            TextEdit::singleline(&mut self.modal_value)
                .desired_width(f32::INFINITY)
                .hint_text("Label name"),
        );
        if ui
            .add_enabled(
                !self.modal_value.trim().is_empty(),
                egui::Button::new("Create Gmail label"),
            )
            .clicked()
        {
            self.command(
                &self.modal_account.clone(),
                Command::CreateLabel(self.modal_value.trim().to_owned()),
            );
            self.modal = None;
        }
    }

    fn schedule(&mut self, ui: &mut Ui) {
        let Some(draft) = self.draft.as_ref() else {
            self.modal = None;
            return;
        };
        ui.label(subject(&draft.subject));
        ui.label(
            self.theme.muted(
                "Uses your local time. Tandem Mail delivers queued mail while it's running.",
            ),
        );
        ui.add(
            TextEdit::singleline(&mut self.modal_value)
                .desired_width(f32::INFINITY)
                .hint_text("YYYY-MM-DD HH:MM"),
        );
        if self.theme.primary(ui, "Queue message").clicked() {
            let time = NaiveDateTime::parse_from_str(self.modal_value.trim(), "%Y-%m-%d %H:%M")
                .ok()
                .and_then(|time| Local.from_local_datetime(&time).single())
                .map(|t| t.timestamp_millis());
            if let Some(time) = time.filter(|time| *time > now() + 10_000) {
                self.modal = None;
                self.queue(time);
            } else {
                self.toast("Enter an unambiguous future date and time", true);
            }
        }
    }
}

fn field(ui: &mut Ui, label: &str, value: &mut String, hint: &str) {
    ui.label(label);
    ui.add(
        TextEdit::singleline(value)
            .desired_width(220.)
            .hint_text(hint),
    );
    ui.end_row();
}

fn flag(ui: &mut Ui, text: &str, tags: &mut Vec<String>, tag: &str) {
    let mut checked = tags.iter().any(|item| item == tag);
    if ui.checkbox(&mut checked, text).changed() {
        if checked {
            tags.push(tag.into());
        } else {
            tags.retain(|item| item != tag);
        }
    }
}

fn account_selector(ui: &mut Ui, id: &str, selected: &mut String, accounts: &[Account]) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(
            accounts
                .iter()
                .find(|a| &a.id == selected)
                .map(|a| a.email.as_str())
                .unwrap_or("Select account"),
        )
        .show_ui(ui, |ui| {
            for account in accounts {
                ui.selectable_value(selected, account.id.clone(), &account.email);
            }
        });
}

fn shortcuts(ui: &mut Ui) {
    Grid::new("keyboard-table")
        .striped(true)
        .spacing([35., 6.])
        .show(ui, |ui| {
            for (keys, description) in [
                ("j / k or ↑ / ↓", "Move through threads"),
                ("o or Enter", "Open thread"),
                ("u or Esc", "Return to list"),
                ("x / Ctrl+A", "Select thread / select page"),
                ("/ or Ctrl+F", "Search"),
                ("Ctrl+K", "Open command menu"),
                ("Ctrl+R", "Refresh mail"),
                ("c", "Compose"),
                ("e / #", "Archive / move to Trash"),
                ("s", "Toggle star"),
                ("Shift+U / Shift+I", "Mark unread / read"),
                ("r / a / f", "Reply / reply all / forward"),
                ("Shift+R", "Draft a reply with Luna"),
                ("Ctrl+Enter", "Queue send"),
                ("Ctrl+Z", "Undo queued send"),
                ("g i", "Open or refresh Inbox"),
                ("g n", "CMU NMRs"),
                ("g a / g s", "All mail / starred"),
                ("g d / g t / g u", "Drafts / sent / unread"),
                ("?", "Keyboard shortcuts"),
            ] {
                ui.label(RichText::new(keys).monospace());
                ui.label(description);
                ui.end_row();
            }
        });
}
