use std::{collections::HashMap, ops::Range};

use eframe::egui::{
    Context, Id, Key, Modifiers, Popup, TextEdit, Ui,
    text::{CCursor, CCursorRange},
};
use lettre::message::Mailbox;

use crate::{mime::parse_mailboxes, model::Account, store::Store};

#[derive(Default)]
pub(super) struct RecipientEditor {
    account: String,
    contacts: Vec<Mailbox>,
    active: Option<Suggestions>,
}

struct Suggestions {
    field: Id,
    query: String,
    selected: usize,
    count: usize,
    dismissed: bool,
}

impl RecipientEditor {
    pub(super) fn dismiss(&mut self, ctx: &Context) -> bool {
        if let Some(active) = &mut self.active
            && !active.dismissed
            && active.count > 0
            && ctx.memory(|mem| mem.has_focus(active.field))
        {
            active.dismissed = true;
            return true;
        }
        false
    }

    pub(super) fn field(
        &mut self,
        ui: &mut Ui,
        label: &str,
        value: &mut String,
        account: Option<&Account>,
        store: &Store,
        focus: bool,
    ) -> bool {
        let id = ui.make_persistent_id(("recipient", label));
        let popup_id = id.with("suggestions");
        let focused = ui.memory(|mem| mem.has_focus(id));
        let mut choose = false;
        if let Some(active) = &mut self.active
            && focused
            && active.field == id
            && active.count > 0
            && !active.dismissed
        {
            ui.input_mut(|input| {
                if input.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    active.selected = (active.selected + 1) % active.count;
                }
                if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    active.selected = (active.selected + active.count - 1) % active.count;
                }
                choose = input.consume_key(Modifiers::NONE, Key::Enter)
                    || input.consume_key(Modifiers::NONE, Key::Tab);
            });
        }
        let hint = if label == "To" {
            "name@example.com"
        } else {
            ""
        };
        let mut edit = TextEdit::singleline(value)
            .hint_text(hint)
            .id(id)
            .desired_width(f32::INFINITY)
            .show(ui);
        if focus {
            edit.response.request_focus();
        }
        let changed = edit.response.changed();
        let pointer_in_menu = self
            .active
            .as_ref()
            .is_some_and(|active| active.field == id)
            && ui.ctx().read_response(popup_id).is_some_and(|popup| {
                ui.ctx()
                    .pointer_hover_pos()
                    .is_some_and(|point| popup.rect.contains(point))
            });
        if !(edit.response.has_focus() || choose || pointer_in_menu) {
            if self
                .active
                .as_ref()
                .is_some_and(|active| active.field == id)
            {
                self.active = None;
            }
            return changed;
        }
        let Some(account) = account else {
            return changed;
        };
        let cursor = edit
            .cursor_range
            .or_else(|| edit.state.cursor.char_range())
            .map_or(value.chars().count(), |range| range.primary.index);
        let token = recipient_at(value, cursor);
        let query = value[token.clone()].trim();
        let query = query
            .rsplit_once('<')
            .map_or(query, |(_, tail)| tail)
            .trim_start_matches('"')
            .trim()
            .to_lowercase();
        if query.is_empty() || query.ends_with('>') {
            self.active = None;
            return changed;
        }
        if account.id != self.account {
            self.contacts = recent_contacts(store, account);
            self.account.clone_from(&account.id);
            self.active = None;
        }

        let existing = [&value[..token.start], &value[token.end..]]
            .into_iter()
            .flat_map(|part| parse_mailboxes(part.trim().trim_matches(',').trim()))
            .collect::<Vec<_>>();
        let mut matches = self
            .contacts
            .iter()
            .filter_map(|contact| {
                let email: &str = contact.email.as_ref();
                if existing
                    .iter()
                    .any(|other| email.eq_ignore_ascii_case(other.email.as_ref()))
                {
                    return None;
                }
                let address = email.to_ascii_lowercase();
                let name = contact.name.as_deref().unwrap_or("").to_lowercase();
                if address == query && value[token.clone()].trim() == email {
                    return None;
                }
                let rank = if address.starts_with(&query) || name.starts_with(&query) {
                    0
                } else if name.split_whitespace().any(|word| word.starts_with(&query)) {
                    1
                } else if address.contains(&query) || name.contains(&query) {
                    2
                } else {
                    return None;
                };
                Some((rank, contact))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|(rank, _)| *rank);
        matches.truncate(7);
        if matches.is_empty() {
            self.active = None;
            return changed;
        }
        if !self
            .active
            .as_ref()
            .is_some_and(|active| active.field == id && active.query == query)
        {
            self.active = Some(Suggestions {
                field: id,
                query,
                selected: 0,
                count: matches.len(),
                dismissed: false,
            });
        }
        let active = self
            .active
            .as_mut()
            .expect("recipient suggestions initialized");
        active.count = matches.len();
        active.selected = active.selected.min(matches.len() - 1);
        if active.dismissed {
            return changed;
        }

        let clicked = Popup::from_response(&edit.response)
            .id(popup_id)
            .open(true)
            .gap(3.)
            .width(edit.response.rect.width().max(240.))
            .show(|ui| {
                matches
                    .iter()
                    .enumerate()
                    .find_map(|(index, (_, contact))| {
                        ui.selectable_label(index == active.selected, contact.to_string())
                            .clicked()
                            .then_some(index)
                    })
            })
            .and_then(|response| response.inner);
        if let Some(index) = clicked.or_else(|| choose.then_some(active.selected)) {
            let address = matches[index].1.to_string();
            let cursor = replace_recipient(value, token, &address);
            edit.state
                .cursor
                .set_char_range(Some(CCursorRange::one(CCursor::new(cursor))));
            edit.state.store(ui.ctx(), id);
            edit.response.request_focus();
            self.active = None;
            true
        } else {
            changed
        }
    }
}

fn recent_contacts(store: &Store, account: &Account) -> Vec<Mailbox> {
    let mut contacts: HashMap<String, (Mailbox, usize)> = HashMap::new();
    for (index, row) in store
        .recent_contact_headers(&account.id)
        .unwrap_or_default()
        .into_iter()
        .enumerate()
    {
        let candidates = if row.sent {
            [row.recipients, row.cc]
        } else {
            [row.sender, String::new()]
        };
        for address in candidates.iter().flat_map(|value| parse_mailboxes(value)) {
            let email = address.email.to_string().to_ascii_lowercase();
            if email.eq_ignore_ascii_case(&account.email) {
                continue;
            }
            contacts
                .entry(email)
                .and_modify(|(contact, rank)| {
                    if row.sent {
                        *rank = (*rank).min(index);
                    }
                    if contact.name.is_none() {
                        contact.name.clone_from(&address.name);
                    }
                })
                .or_insert((address, if row.sent { index } else { 1200 + index }));
        }
    }
    let mut contacts = contacts.into_values().collect::<Vec<_>>();
    contacts.sort_by(|(a, rank_a), (b, rank_b)| {
        let a: &str = a.email.as_ref();
        rank_a.cmp(rank_b).then_with(|| a.cmp(b.email.as_ref()))
    });
    contacts.into_iter().map(|(address, _)| address).collect()
}

fn recipient_at(value: &str, cursor: usize) -> Range<usize> {
    let at = value
        .char_indices()
        .nth(cursor)
        .map_or(value.len(), |(at, _)| at);
    let (mut start, mut quoted, mut escaped, mut bracketed) = (0, false, false, false);
    for (index, ch) in value.char_indices() {
        if std::mem::take(&mut escaped) {
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '<' if !quoted => bracketed = true,
            '>' if !quoted => bracketed = false,
            ',' if !quoted && !bracketed => {
                if index >= at {
                    return start..index;
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    start..value.len()
}

fn replace_recipient(value: &mut String, token: Range<usize>, address: &str) -> usize {
    let leading_space = if token.start == 0 { "" } else { " " };
    let inserted = format!("{leading_space}{address}");
    let cursor = value[..token.start].chars().count() + inserted.chars().count();
    value.replace_range(token, &inserted);
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_the_recipient_under_the_unicode_cursor_and_keeps_quoted_commas() {
        let mut value = String::from("\"García, Bea\" <bea@example.org>, Jac, later@example.org");
        let cursor = value[..value.find("Jac").unwrap() + 2].chars().count();
        let token = recipient_at(&value, cursor);
        assert_eq!(&value[token.clone()], " Jac");
        replace_recipient(&mut value, token, "Jack <jack@example.org>");
        assert_eq!(
            value,
            "\"García, Bea\" <bea@example.org>, Jack <jack@example.org>, later@example.org"
        );
    }
}
