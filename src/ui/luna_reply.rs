use eframe::egui::{self, ScrollArea, TextEdit, Ui};
use tokio::sync::oneshot;

use super::{MailApp, Modal, subject};
use crate::{
    luna::{self, Reply, Request},
    mime,
    model::{Account, Message, ThreadKey},
};

type Job = oneshot::Receiver<Result<Reply, String>>;

pub(super) struct LunaDialog {
    account: Account,
    key: ThreadKey,
    target: Message,
    thread: Vec<Message>,
    instructions: String,
    examples: String,
    use_sent: bool,
    pending: Option<Job>,
    result: Option<Reply>,
    error: Option<String>,
}

impl MailApp {
    pub(super) fn start_luna(&mut self, target: Option<&Message>) {
        if self.demo {
            self.toast("Luna is unavailable in the sample mailbox", false);
            return;
        }
        if self.draft.is_some() {
            self.toast("Close your current draft first", false);
            return;
        }
        let Some(key) = self.open.clone() else {
            return;
        };
        let Some(account) = self.account(&key.account).cloned() else {
            return;
        };
        let Some(index) = self.messages.iter().rposition(|message| {
            target.map_or_else(
                || !message.labels.iter().any(|tag| tag == "DRAFT"),
                |target| target.id == message.id,
            )
        }) else {
            return;
        };
        let target = self.messages[index].clone();
        let thread = self.messages[index.saturating_sub(7)..=index]
            .iter()
            .filter(|message| !message.labels.iter().any(|tag| tag == "DRAFT"))
            .cloned()
            .collect();
        self.set_modal(Modal::Luna);
        self.luna = Some(LunaDialog {
            account,
            key,
            thread,
            target,
            instructions: String::new(),
            examples: String::new(),
            use_sent: true,
            pending: None,
            result: None,
            error: None,
        });
    }

    pub(super) fn luna_dialog(&mut self, ui: &mut Ui) {
        let Some(state) = self.luna.as_mut() else {
            return;
        };
        if let Some(pending) = &mut state.pending {
            match pending.try_recv() {
                Ok(Ok(reply)) => {
                    state.result = Some(reply);
                    state.pending = None;
                }
                Ok(Err(error)) => {
                    state.error = Some(error);
                    state.pending = None;
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    state.error = Some("Luna stopped before returning a reply".into());
                    state.pending = None;
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
        }

        ui.label(egui::RichText::new(subject(&state.target.subject)).strong());
        ui.label(self.theme.muted(format!("From {}", state.account.email)));
        ui.label("GPT-6 Luna receives this thread and any writing samples or relevant excerpts from this account. Tandem Mail calls your signed-in Codex CLI.");
        ui.add_space(5.);
        ui.add_enabled_ui(state.pending.is_none(), |ui| {
            ui.add(
                TextEdit::multiline(&mut state.instructions)
                    .hint_text("What should the reply say?")
                    .desired_rows(2)
                    .desired_width(f32::INFINITY),
            );
            ui.checkbox(
                &mut state.use_sent,
                "Use recent sent mail as writing samples",
            );
            egui::CollapsingHeader::new("Add your own writing samples").show(ui, |ui| {
                ui.add(
                    TextEdit::multiline(&mut state.examples)
                        .hint_text("Paste examples of emails you’ve written")
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
            });
        });
        ui.add_space(5.);

        if state.pending.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Luna is checking context and drafting…");
                if ui.button("Cancel").clicked() {
                    state.pending = None;
                }
            });
        } else if self
            .theme
            .primary(
                ui,
                if state.result.is_some() {
                    "Regenerate"
                } else {
                    "Generate reply"
                },
            )
            .clicked()
        {
            let request = Request {
                account: state.account.clone(),
                key: state.key.clone(),
                target: state.target.clone(),
                thread: state.thread.clone(),
                instructions: state.instructions.clone(),
                examples: state.examples.clone(),
                use_sent: state.use_sent,
            };
            let (mut send, receive) = oneshot::channel();
            let ctx = ui.ctx().clone();
            let path = self.path.clone();
            self.runtime.spawn(async move {
                let result = tokio::select! {
                    _ = send.closed() => return,
                    result = luna::draft(path, request) => result,
                };
                let _ = send.send(result.map_err(|error| error.to_string()));
                ctx.request_repaint();
            });
            state.pending = Some(receive);
            state.result = None;
            state.error = None;
        }
        if let Some(error) = &state.error {
            ui.colored_label(self.theme.error, error);
        }
        let Some(reply) = &mut state.result else {
            return;
        };
        ui.separator();
        if !reply.needs_input.is_empty() {
            ui.label(egui::RichText::new("Needs your input").strong());
            for note in &reply.needs_input {
                ui.label(note);
            }
        }
        ui.label(self.theme.muted(format!(
            "{} from Sent · {} related",
            reply.sent_examples,
            reply.source_subjects.len()
        )));
        if !reply.source_subjects.is_empty() {
            egui::CollapsingHeader::new("Related email").show(ui, |ui| {
                for subject in &reply.source_subjects {
                    ui.label(subject);
                }
            });
        }
        if reply.text.is_empty() {
            return;
        }
        ScrollArea::vertical().max_height(210.).show(ui, |ui| {
            ui.add(
                TextEdit::multiline(&mut reply.text)
                    .desired_rows(5)
                    .desired_width(f32::INFINITY),
            );
        });
        let use_reply = ui
            .horizontal(|ui| {
                if ui.button("Copy reply").clicked() {
                    ui.ctx().copy_text(reply.text.clone());
                }
                ui.button("Use in reply").clicked()
            })
            .inner;
        if use_reply {
            let mut draft = mime::reply(&state.target, &state.account, &state.key.thread, false);
            draft.body = reply.text.clone();
            draft.formatting.clear();
            self.modal = None;
            self.luna = None;
            self.open_compose(draft);
        }
    }
}
