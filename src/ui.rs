mod body;
mod compose_editor;
mod dialogs;
mod fixtures;
mod labels;
mod luna_reply;
mod recipients;
mod theme;
mod workspace;

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

use eframe::egui::{Context, Id, Key, Modifiers};

use crate::{
    mime,
    model::{
        Account, Action, Draft, DraftFile, DraftFileSource, Filter, Label, Message, Row, ThreadKey,
    },
    store::{Download, SavedSearch, Store, View},
    sync::{self, Command, Commands, Event, Status},
};
pub use fixtures::seed_demo;
pub(crate) use theme::Theme;

const PAGE: usize = 80;

#[derive(Clone, Copy)]
enum SaveLocation {
    Downloads,
    ChooseFolder,
}

enum DownloadFiles {
    Single(Download, String),
    Attachments(Vec<(usize, String)>),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Modal {
    Search,
    Filters,
    Settings,
    Keys,
    SaveSearch,
    CreateLabel,
    Schedule,
    Palette,
    Luna,
}

#[derive(Default)]
struct SearchForm {
    from: String,
    to: String,
    subject: String,
    words: String,
    excludes: String,
    before: String,
    after: String,
    filename: String,
    size: String,
    attachment: bool,
    unread: bool,
    starred: bool,
}

#[derive(Default)]
struct Online {
    generation: u64,
    query: String,
    pending: HashSet<String>,
    cursors: HashMap<String, String>,
    threads: Vec<ThreadKey>,
    errors: HashMap<String, String>,
}

pub struct MailApp {
    store: Store,
    accounts: Vec<Account>,
    scope: Option<String>,
    path: PathBuf,
    runtime: tokio::runtime::Runtime,
    workers: HashMap<String, Commands>,
    events: mpsc::Receiver<Event>,
    event_tx: mpsc::Sender<Event>,
    statuses: HashMap<String, Status>,
    theme: Theme,
    demo: bool,
    view: View,
    query: String,
    search_form: SearchForm,
    online: Online,
    rows: Vec<Row>,
    labels: Vec<(String, Label)>,
    saved: Vec<SavedSearch>,
    selected: BTreeSet<ThreadKey>,
    cursor: usize,
    offset: usize,
    scroll_cursor: bool,
    open: Option<ThreadKey>,
    messages: Vec<Message>,
    reader_content: body::ReaderContent,
    expanded: HashSet<String>,
    draft: Option<Draft>,
    draft_focus: bool,
    composer: compose_editor::Editor,
    recipients: recipients::RecipientEditor,
    luna: Option<luna_reply::LunaDialog>,
    modal: Option<Modal>,
    modal_account: String,
    modal_value: String,
    filter: Filter,
    filter_size: String,
    filter_view: Vec<Filter>,
    confirm_filter: Option<String>,
    modal_focus: bool,
    recent_archives: HashMap<ThreadKey, Instant>,
    notify: Option<(String, bool, Instant)>,
    errors: Option<String>,
    dirty: bool,
    loaded_at: Instant,
    g_prefix: Option<Instant>,
    last_queued: Option<i64>,
}

impl MailApp {
    pub fn new(
        ctx: &Context,
        store: Store,
        path: PathBuf,
        accounts: Vec<Account>,
        runtime: tokio::runtime::Runtime,
        demo: bool,
    ) -> Self {
        let theme = Theme::new(store.dark().unwrap_or(true));
        theme.apply(ctx);
        body::load_font(ctx);
        let (event_tx, events) = mpsc::channel();
        let mut statuses = HashMap::new();
        for account in &accounts {
            if demo {
                statuses.insert(
                    account.id.clone(),
                    Status {
                        message: "Sample mailbox".into(),
                        failed: false,
                        last_sync: None,
                    },
                );
            } else {
                let time = store.state(&account.id).ok().and_then(|s| s.last_sync);
                statuses.insert(
                    account.id.clone(),
                    Status {
                        message: "Connecting to Gmail".into(),
                        failed: false,
                        last_sync: time,
                    },
                );
            }
        }
        let modal_account = accounts.first().map(|a| a.id.clone()).unwrap_or_default();
        let mut app = Self {
            store,
            accounts,
            scope: None,
            path,
            runtime,
            workers: HashMap::new(),
            events,
            event_tx,
            statuses,
            theme,
            demo,
            view: View::Inbox,
            query: String::new(),
            search_form: SearchForm::default(),
            online: Online::default(),
            rows: vec![],
            labels: vec![],
            saved: vec![],
            selected: BTreeSet::new(),
            cursor: 0,
            offset: 0,
            scroll_cursor: false,
            open: None,
            messages: vec![],
            reader_content: body::ReaderContent::default(),
            expanded: HashSet::new(),
            draft: None,
            draft_focus: false,
            composer: compose_editor::Editor::default(),
            recipients: recipients::RecipientEditor::default(),
            luna: None,
            modal: None,
            modal_account,
            modal_value: String::new(),
            filter: Filter::default(),
            filter_size: String::new(),
            filter_view: vec![],
            confirm_filter: None,
            modal_focus: false,
            recent_archives: HashMap::new(),
            notify: None,
            errors: None,
            dirty: false,
            loaded_at: Instant::now(),
            g_prefix: None,
            last_queued: None,
        };
        app.reload();
        if !demo {
            app.workers = app
                .accounts
                .iter()
                .map(|account| {
                    (
                        account.id.clone(),
                        sync::launch(
                            &app.runtime,
                            app.path.clone(),
                            account.clone(),
                            app.event_tx.clone(),
                            ctx.clone(),
                        ),
                    )
                })
                .collect();
        }
        app
    }

    fn account(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }
    fn default_account(&self) -> String {
        self.scope
            .clone()
            .or_else(|| self.accounts.first().map(|a| a.id.clone()))
            .unwrap_or_default()
    }

    fn current_accounts(&self) -> Vec<String> {
        self.accounts
            .iter()
            .filter(|a| self.scope.as_ref().is_none_or(|id| &a.id == id))
            .map(|a| a.id.clone())
            .collect()
    }

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.notify = Some((text.into(), error, Instant::now()));
    }

    fn command(&mut self, account: &str, command: Command) {
        if let Some(worker) = self.workers.get(account) {
            if worker.send(command).is_err() {
                self.toast("Gmail worker is unavailable", true);
            }
        } else {
            self.toast("Gmail actions are disabled in the sample mailbox", false);
        }
    }

    fn process_events(&mut self, ctx: &Context) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Status(id, status) => {
                    self.statuses.insert(id, status);
                }
                Event::Changed => self.dirty = true,
                Event::Feedback(message, error) => {
                    self.toast(message, error);
                    self.dirty = true;
                }
                Event::Search {
                    account,
                    generation,
                    threads,
                    cursor,
                    error,
                } if generation == self.online.generation => {
                    self.online.pending.remove(&account);
                    if let Some(error) = error {
                        self.online.errors.insert(account.clone(), error);
                    }
                    if let Some(cursor) = cursor {
                        self.online.cursors.insert(account, cursor);
                    }
                    self.online.threads.extend(threads);
                    self.online.threads.sort_unstable();
                    self.online.threads.dedup();
                    self.dirty = true;
                }
                Event::Search { .. } => {}
            }
        }
        if self.dirty {
            let cooldown = Duration::from_millis(300);
            if self.loaded_at.elapsed() >= cooldown {
                self.reload();
            } else {
                ctx.request_repaint_after(cooldown - self.loaded_at.elapsed());
            }
        }
        if self
            .notify
            .as_ref()
            .is_some_and(|(_, _, time)| time.elapsed() > Duration::from_secs(12))
        {
            self.notify = None;
        }
    }

    fn reload(&mut self) {
        let cursor_key = self.rows.get(self.cursor).map(|row| row.key.clone());
        match self.store.list(
            &self.view,
            &self.current_accounts(),
            &self.online.threads,
            PAGE + 1,
            self.offset,
        ) {
            Ok(rows) => {
                self.rows = rows;
                self.errors = None;
            }
            Err(error) => {
                self.rows.clear();
                self.errors = Some(error.to_string());
            }
        }
        if let Some(index) = self
            .rows
            .iter()
            .take(PAGE)
            .position(|row| Some(&row.key) == cursor_key.as_ref())
        {
            self.cursor = index;
        } else {
            self.cursor = self.cursor.min(self.rows.len().min(PAGE).saturating_sub(1));
        }
        self.labels = self.store.labels().unwrap_or_default();
        self.saved = self.store.saved_searches().unwrap_or_default();
        if let Some(key) = &self.open {
            match self.store.thread(key) {
                Ok(messages) => self.messages = messages,
                Err(error) => self.toast(error.to_string(), true),
            }
        }
        if self.modal == Some(Modal::Filters) {
            self.filter_view = self.store.filters(&self.modal_account).unwrap_or_default();
        }
        if !self.demo {
            let accounts: Vec<_> = self
                .accounts
                .iter()
                .map(|account| account.id.as_str())
                .collect();
            if let Ok(inbox) = self.store.inbox_reader_keys(&accounts) {
                self.reader_content.set_inbox(inbox);
            } else {
                self.reader_content.suspend_warming();
            }
        }
        self.loaded_at = Instant::now();
        self.dirty = false;
    }

    fn set_view(&mut self, view: View) {
        self.view = view;
        self.open = None;
        self.messages.clear();
        self.selected.clear();
        self.rows.clear();
        self.cursor = 0;
        self.offset = 0;
        self.scroll_cursor = true;
        self.reload();
    }

    fn go(&mut self, view: View) {
        self.query.clear();
        self.set_view(view);
    }

    fn inbox_shortcut(&mut self) {
        if self.view == View::Inbox && self.open.is_none() {
            self.reload();
            self.refresh();
        } else {
            self.go(View::Inbox);
        }
    }

    fn go_to_nmrs(&mut self) {
        let label = self.labels.iter().find(|(account, label)| {
            account == "cmu" && label.kind == "user" && label.name.eq_ignore_ascii_case("NMRs")
        });
        if let Some((account, label)) = label {
            let target = View::Tag(label.id.clone());
            self.scope = Some(account.clone());
            self.go(target);
        } else {
            self.toast("The CMU NMRs label is unavailable", true);
        }
    }

    fn local_search(&mut self, ctx: &Context) {
        let query = self.query.trim().to_owned();
        self.set_view(if query.is_empty() {
            View::Inbox
        } else {
            View::Search(query)
        });
        ctx.memory_mut(|memory| memory.surrender_focus(Id::new("global-search")));
    }

    fn gmail_search(&mut self) {
        if self.demo {
            self.toast("Try offline search in the sample mailbox", false);
            return;
        }
        let query = self.query.trim().to_owned();
        if query.is_empty() {
            self.toast("Enter a query to search Gmail", false);
            return;
        }
        self.online = Online {
            generation: self.online.generation.wrapping_add(1),
            query: query.clone(),
            ..Online::default()
        };
        self.set_view(View::Online);
        for account in self.current_accounts() {
            self.dispatch_search(account, query.clone(), None);
        }
    }

    fn dispatch_search(&mut self, id: String, query: String, cursor: Option<String>) {
        self.online.pending.insert(id.clone());
        self.command(
            &id,
            Command::Search {
                generation: self.online.generation,
                query,
                cursor,
            },
        );
    }

    fn grab_online_page(&mut self) {
        let query = self.online.query.clone();
        if query.is_empty() {
            return;
        }
        let cursors = std::mem::take(&mut self.online.cursors);
        self.online.threads.clear();
        self.set_view(View::Online);
        for (id, cursor) in cursors {
            self.dispatch_search(id, query.clone(), Some(cursor));
        }
    }

    fn set_scope(&mut self, scope: Option<String>) {
        self.scope = scope;
        if self.view == View::Online {
            self.gmail_search();
        } else if matches!(self.view, View::Tag(ref tag) if tag.starts_with("Label_")) {
            self.go(View::Inbox);
        } else {
            self.set_view(self.view.clone());
        }
    }

    fn open_row(&mut self, index: usize) {
        let Some(row) = self.rows.get(index) else {
            return;
        };
        let key = row.key.clone();
        let unread = row.unread;
        self.cursor = index;
        self.selected.clear();
        self.open = Some(key.clone());
        self.expanded.clear();
        self.messages = match self.store.thread(&key) {
            Ok(messages) => messages,
            Err(error) => {
                self.toast(error.to_string(), true);
                vec![]
            }
        };
        if let Some(message) = self
            .messages
            .iter()
            .rev()
            .find(|item| !item.labels.iter().any(|tag| tag == "DRAFT"))
            .or_else(|| self.messages.last())
        {
            self.expanded.insert(message.id.clone());
        }
        if unread && !self.demo && !matches!(self.view, View::Drafts) {
            self.change(vec![key], Action::labels(&[], &["UNREAD"]));
        }
    }

    fn change(&mut self, keys: Vec<ThreadKey>, action: Action) -> bool {
        if keys.is_empty() {
            return false;
        }
        if action.changes_mailbox() {
            if self.demo {
                self.toast("Gmail actions are disabled in the sample mailbox", false);
                return false;
            }
            match self.store.queue_mailbox_changes(&keys, &action) {
                Ok(queued) if queued.is_empty() => self.toast(
                    if action.inbox_override() == Some(false) {
                        "Already archived"
                    } else {
                        "Already in Inbox"
                    },
                    false,
                ),
                Ok(queued) => {
                    if action.inbox_override() == Some(false) {
                        let archived_at = Instant::now();
                        for key in queued {
                            self.recent_archives.insert(key, archived_at);
                        }
                    } else {
                        for key in queued {
                            self.recent_archives.remove(&key);
                        }
                        self.toast("Mailbox change queued", false);
                    }
                    self.reload();
                    self.scroll_cursor = true;
                }
                Err(error) => {
                    self.toast(format!("Could not queue mailbox change: {error}"), true);
                    return false;
                }
            }
            return true;
        }
        let mut grouped: HashMap<String, Vec<ThreadKey>> = HashMap::new();
        for key in keys {
            grouped.entry(key.account.clone()).or_default().push(key);
        }
        for (id, keys) in grouped {
            self.command(
                &id,
                Command::Change {
                    keys,
                    action: action.clone(),
                },
            );
        }
        true
    }

    fn targets(&self) -> Vec<ThreadKey> {
        if !self.selected.is_empty() {
            return self.selected.iter().cloned().collect();
        }
        if let Some(key) = &self.open {
            return vec![key.clone()];
        }
        self.rows
            .get(self.cursor)
            .map(|r| vec![r.key.clone()])
            .unwrap_or_default()
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let cursor = self.cursor.saturating_add_signed(delta);
        if cursor >= PAGE && self.rows.len() > PAGE {
            self.page(PAGE as isize);
        } else if self.cursor == 0 && delta < 0 && self.offset > 0 {
            self.page(-(PAGE as isize));
            self.cursor = PAGE - 1;
        } else {
            self.cursor = cursor.min(self.rows.len().min(PAGE) - 1);
        }
        self.scroll_cursor = true;
        if self.open.is_some() {
            self.open_row(self.cursor);
        }
    }

    fn page(&mut self, delta: isize) {
        self.offset = self.offset.saturating_add_signed(delta);
        self.rows.clear();
        self.cursor = 0;
        self.scroll_cursor = true;
        self.reload();
    }

    fn toggle_selected(&mut self, key: ThreadKey) {
        if !self.selected.remove(&key) {
            self.selected.insert(key);
        }
    }

    fn open_compose(&mut self, draft: Draft) {
        if self.draft.is_some() {
            self.toast("Close your current draft first", false);
            return;
        }
        self.draft = Some(draft);
        self.draft_focus = true;
        self.recipients = recipients::RecipientEditor::default();
    }

    fn new_compose(&mut self) {
        if self.draft.is_none() {
            self.open_compose(Draft {
                account: self.default_account(),
                ..Draft::default()
            });
        }
    }

    fn reply_to(&mut self, message: Option<Message>, mode: u8) {
        let Some(message) = message.or_else(|| {
            self.messages
                .iter()
                .rev()
                .find(|item| !item.labels.iter().any(|tag| tag == "DRAFT"))
                .cloned()
        }) else {
            return;
        };
        let Some(key) = self.open.clone() else {
            return;
        };
        let Some(account) = self.account(&key.account) else {
            return;
        };
        let draft = if mode == 2 {
            Draft {
                account: account.id.clone(),
                subject: format!("Fwd: {}", subject(&message.subject)),
                files: cached_files(&key, &message),
                body: format!(
                    "\n\n---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {}\nTo: {}\n\n{}",
                    message.sender,
                    crate::model::date_label(message.date, true),
                    message.subject,
                    message.recipients,
                    message.body
                ),
                ..Draft::default()
            }
        } else {
            mime::reply(&message, account, &key.thread, mode == 1)
        };
        self.open_compose(draft);
    }

    fn snooze(&mut self, millis: i64) {
        for key in self.targets() {
            if let Err(error) = self.store.snooze(&key, millis) {
                self.toast(error.to_string(), true);
            }
        }
        self.selected.clear();
        self.open = None;
        self.reload();
    }

    fn refresh(&mut self) {
        for id in self.current_accounts() {
            if let Some(status) = self.statuses.get_mut(&id) {
                status.message = "Refreshing Gmail".into();
            }
            self.command(&id, Command::Refresh);
        }
    }

    fn save_file(
        &self,
        ctx: &Context,
        key: &ThreadKey,
        id: &str,
        part: Download,
        name: &str,
        location: SaveLocation,
    ) {
        self.save_files(
            ctx,
            key,
            id,
            DownloadFiles::Single(part, name.to_owned()),
            location,
        );
    }

    fn save_files(
        &self,
        ctx: &Context,
        key: &ThreadKey,
        id: &str,
        files: DownloadFiles,
        location: SaveLocation,
    ) {
        if self.demo {
            return;
        }
        let id = id.to_owned();
        let key = key.clone();
        let cache = self.path.clone();
        let events = self.event_tx.clone();
        let ctx = ctx.clone();
        self.runtime.spawn(async move {
            let downloads = dirs::download_dir()
                .or_else(|| dirs::home_dir().map(|home| home.join("Downloads")));
            let directory = match location {
                SaveLocation::Downloads => downloads,
                SaveLocation::ChooseFolder => {
                    let mut dialog =
                        rfd::AsyncFileDialog::new().set_title("Choose download folder");
                    if let Some(folder) = downloads.filter(|folder| folder.is_dir()) {
                        dialog = dialog.set_directory(folder);
                    }
                    let Some(folder) = dialog.pick_folder().await else {
                        return;
                    };
                    Some(folder.path().to_owned())
                }
            };
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Event> {
                let directory = directory
                    .ok_or_else(|| anyhow::anyhow!("Could not find your Downloads folder"))?;
                let store = Store::open_read_only(&cache)?;
                if matches!(location, SaveLocation::Downloads) {
                    std::fs::create_dir_all(&directory)?;
                }
                let event = match files {
                    DownloadFiles::Single(part, name) => {
                        let data = store.download(&key.account, &id, &part)?;
                        let path = mime::save_download(&directory, &name, &data)?;
                        Event::Feedback(format!("Saved to {}", path.display()), false)
                    }
                    DownloadFiles::Attachments(files) => {
                        let raw = store.download(&key.account, &id, &Download::Raw)?;
                        let results = mime::save_attachments(&raw, &directory, &files)?;
                        let saved = results.iter().filter(|result| result.is_ok()).count();
                        if let Some(error) = results.iter().find_map(|result| result.as_ref().err())
                        {
                            Event::Feedback(
                                format!("Saved {saved} of {} attachments. {error:#}", files.len()),
                                true,
                            )
                        } else {
                            Event::Feedback(
                                format!("Saved {saved} attachments to {}", directory.display()),
                                false,
                            )
                        }
                    }
                };
                Ok(event)
            })
            .await;
            let event = match result {
                Ok(Ok(event)) => event,
                Ok(Err(error)) => Event::Feedback(format!("Download failed. {error:#}"), true),
                Err(error) => Event::Feedback(error.to_string(), true),
            };
            let _ = events.send(event);
            ctx.request_repaint();
        });
    }

    fn undo_send(&mut self, id: i64) {
        match self.store.cancel_send(id) {
            Ok(Some(draft)) => {
                if self.draft.is_none() {
                    self.open_compose(draft);
                }
                self.toast("Returned to drafts", false);
                if self.last_queued == Some(id) {
                    self.last_queued = None;
                }
            }
            Ok(None) => self.toast("The message has already left the queue", false),
            Err(error) => self.toast(error.to_string(), true),
        }
    }

    fn keys(&mut self, ctx: &Context) {
        if self.draft.is_some() && self.composer.editing_link() {
            return;
        }
        let key = |mods, target| {
            ctx.input_mut(|input| {
                let mut hit = false;
                input.events.retain(|event| {
                    if let eframe::egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        repeat,
                        ..
                    } = event
                        && *key == target
                        && modifiers.matches_exact(mods)
                        && (target != Key::E || !repeat)
                    {
                        hit = true;
                        return false;
                    }
                    true
                });
                hit
            })
        };
        let character = |target: &str| {
            ctx.input_mut(|input| {
                let mut hit = false;
                input.events.retain(|event| {
                    if matches!(event, eframe::egui::Event::Text(text) if text == target) {
                        hit = true;
                        return false;
                    }
                    true
                });
                hit
            })
        };
        if self.draft.is_some() && self.modal.is_none() && key(Modifiers::COMMAND, Key::Enter) {
            self.queue(crate::store::now() + 10_000);
            return;
        }
        if key(Modifiers::COMMAND, Key::R) {
            self.refresh();
        }
        if self.draft.is_none() && key(Modifiers::COMMAND, Key::K) {
            self.set_modal(Modal::Palette);
        }
        if key(Modifiers::NONE, Key::Escape) {
            if let Some(modal) = self.modal.take() {
                if modal == Modal::Luna {
                    self.luna = None;
                }
                return;
            }
            if self.draft.is_some() && self.recipients.dismiss(ctx) {
                return;
            }
            if self.draft.take().is_some() {
                return;
            }
            if ctx.memory(|m| m.has_focus(Id::new("global-search"))) {
                ctx.memory_mut(|m| m.surrender_focus(Id::new("global-search")));
            } else if !self.selected.is_empty() {
                self.selected.clear();
            } else {
                self.open = None;
            }
            return;
        }
        if self.modal.is_some()
            || self.draft.is_some()
            || ctx.memory(|m| m.has_focus(Id::new("global-search")))
        {
            return;
        }
        if key(Modifiers::SHIFT, Key::Questionmark)
            || key(Modifiers::SHIFT, Key::Slash)
            || character("?")
        {
            self.set_modal(Modal::Keys);
            return;
        }
        if key(Modifiers::NONE, Key::Slash) || key(Modifiers::COMMAND, Key::F) {
            character("/");
            ctx.memory_mut(|m| m.request_focus(Id::new("global-search")));
            return;
        }
        if key(Modifiers::COMMAND, Key::A) && self.open.is_none() {
            self.selected
                .extend(self.rows.iter().take(PAGE).map(|r| r.key.clone()));
            return;
        }
        let starts_go = key(Modifiers::NONE, Key::G);
        if starts_go {
            self.g_prefix = Some(Instant::now());
        }
        if self
            .g_prefix
            .is_some_and(|start| start.elapsed() < Duration::from_secs(1))
        {
            if key(Modifiers::NONE, Key::I) {
                self.g_prefix = None;
                self.inbox_shortcut();
                return;
            }
            if key(Modifiers::NONE, Key::N) {
                self.g_prefix = None;
                self.go_to_nmrs();
                return;
            }
            for (k, view) in [
                (Key::A, View::All),
                (Key::S, View::Tag("STARRED".into())),
                (Key::D, View::Drafts),
                (Key::T, View::Tag("SENT".into())),
                (Key::U, View::Tag("UNREAD".into())),
            ] {
                if key(Modifiers::NONE, k) {
                    self.g_prefix = None;
                    self.go(view);
                    return;
                }
            }
        }
        if starts_go {
            return;
        }
        if key(Modifiers::NONE, Key::C) {
            self.new_compose();
        } else if key(Modifiers::NONE, Key::J) || key(Modifiers::NONE, Key::ArrowDown) {
            self.move_cursor(1);
        } else if key(Modifiers::NONE, Key::K) || key(Modifiers::NONE, Key::ArrowUp) {
            self.move_cursor(-1);
        } else if key(Modifiers::NONE, Key::O) || key(Modifiers::NONE, Key::Enter) {
            self.open_row(self.cursor);
        } else if key(Modifiers::NONE, Key::X) {
            if let Some(row) = self.rows.get(self.cursor) {
                self.toggle_selected(row.key.clone());
            }
        } else if key(Modifiers::NONE, Key::E) {
            self.action(Action::labels(&[], &["INBOX"]), true);
        } else if key(Modifiers::SHIFT, Key::Num3) || character("#") {
            self.action(Action::Trash, true);
        } else if key(Modifiers::SHIFT, Key::U) {
            self.action(Action::labels(&["UNREAD"], &[]), true);
        } else if key(Modifiers::SHIFT, Key::I) {
            self.action(Action::labels(&[], &["UNREAD"]), false);
        } else if key(Modifiers::NONE, Key::S) {
            let starred = self.rows.get(self.cursor).is_some_and(|row| row.starred);
            self.action(
                if starred {
                    Action::labels(&[], &["STARRED"])
                } else {
                    Action::labels(&["STARRED"], &[])
                },
                false,
            );
        } else if key(Modifiers::NONE, Key::U) {
            self.open = None;
        } else if key(Modifiers::SHIFT, Key::R) {
            self.start_luna(None);
        } else if key(Modifiers::NONE, Key::R) {
            self.reply_to(None, 0);
        } else if key(Modifiers::NONE, Key::A) {
            self.reply_to(None, 1);
        } else if key(Modifiers::NONE, Key::F) {
            self.reply_to(None, 2);
        } else if key(Modifiers::COMMAND, Key::Z)
            && let Some(id) = self.last_queued
        {
            self.undo_send(id);
        }
    }

    fn action(&mut self, action: Action, close: bool) {
        if !self.change(self.targets(), action) {
            return;
        }
        self.selected.clear();
        if close {
            self.open = None;
        }
    }

    fn set_modal(&mut self, modal: Modal) {
        if modal != Modal::Luna {
            self.luna = None;
        }
        self.modal = Some(modal);
        self.modal_focus = true;
        self.modal_value.clear();
        self.confirm_filter = None;
        if matches!(modal, Modal::Filters | Modal::CreateLabel) {
            self.modal_account = self.default_account();
        }
        if modal == Modal::Filters {
            self.filter_view = self.store.filters(&self.modal_account).unwrap_or_default();
            self.command(&self.modal_account.clone(), Command::ReadFilters);
        }
    }
}

impl MailApp {
    #[cfg(target_os = "linux")]
    pub(crate) fn reader_is_covered(&self) -> bool {
        self.modal.is_some() || (self.draft.is_some() && !self.inline_compose())
    }

    fn frame(&mut self, ctx: &Context) {
        self.process_events(ctx);
        self.keys(ctx);
        self.reader_content.poll(
            self.open.as_ref().map(|key| key.account.as_str()),
            &self.expanded,
        );
        self.header(ctx);
        self.sidebar(ctx);
        self.footer(ctx);
        self.workspace(ctx);
        if !self.demo {
            self.reader_content.warm(ctx, &self.runtime, &self.path);
        }
        self.archive_undo(ctx);
        self.compose(ctx);
        self.dialogs(ctx);
        ctx.request_repaint_after(Duration::from_secs(1));
        if self.loaded_at.elapsed() > Duration::from_secs(30) {
            self.reload();
        }
    }
}

impl eframe::App for MailApp {
    fn update(&mut self, ctx: &Context, _: &mut eframe::Frame) {
        self.frame(ctx);
    }
}

fn cached_files(key: &ThreadKey, message: &Message) -> Vec<DraftFile> {
    message
        .files
        .iter()
        .map(|(index, name)| DraftFile {
            name: name.clone(),
            source: DraftFileSource::Cached {
                account: key.account.clone(),
                message: message.id.clone(),
                index: *index,
            },
        })
        .collect()
}

fn subject(text: &str) -> &str {
    if text.trim().is_empty() {
        "(No subject)"
    } else {
        text
    }
}

fn name(sender: &str) -> &str {
    if let Some((display, _)) = sender.split_once(" <") {
        let display = display.trim_matches('"').trim();
        if !display.is_empty() {
            return display;
        }
    }
    if sender.trim().is_empty() {
        "Unknown sender"
    } else {
        sender
    }
}

fn tag_name(tag: &str) -> &str {
    match tag {
        "STARRED" => "Starred",
        "UNREAD" => "Unread",
        "SENT" => "Sent",
        "SPAM" => "Spam",
        "TRASH" => "Trash",
        "CATEGORY_PERSONAL" => "Primary",
        "CATEGORY_SOCIAL" => "Social",
        "CATEGORY_PROMOTIONS" => "Promotions",
        "CATEGORY_UPDATES" => "Updates",
        "CATEGORY_FORUMS" => "Forums",
        _ => tag,
    }
}

#[cfg(test)]
mod tests {
    use eframe::egui::{self, Event, Key, Modifiers, RawInput, Rect, pos2, vec2};

    use super::{MailApp, Modal, View, seed_demo};
    use crate::{model::Label, store::Store, sync::Command};

    #[test]
    fn compose_shortcuts_format_text_and_open_the_link_dialog() -> anyhow::Result<()> {
        let path = std::path::PathBuf::from(":memory:");
        let ctx = egui::Context::default();
        let mut app = MailApp::new(
            &ctx,
            Store::open(&path)?,
            path,
            vec![],
            tokio::runtime::Runtime::new()?,
            true,
        );
        app.open_compose(crate::model::Draft {
            to: "reader@example.com".into(),
            body: "Read more".into(),
            ..Default::default()
        });
        let run = |app: &mut MailApp, events| {
            let _ = ctx.run(
                RawInput {
                    screen_rect: Some(Rect::from_min_size(pos2(0., 0.), vec2(1200., 900.))),
                    events,
                    ..Default::default()
                },
                |ctx| app.frame(ctx),
            );
        };
        let key = |key, modifiers| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        run(&mut app, vec![]);
        run(&mut app, vec![key(Key::A, Modifiers::COMMAND)]);
        run(&mut app, vec![key(Key::B, Modifiers::COMMAND)]);
        assert!(app.draft.as_ref().unwrap().formatting[0].style.bold);
        run(&mut app, vec![key(Key::K, Modifiers::COMMAND)]);
        assert!(app.composer.editing_link());
        run(&mut app, vec![Event::Text("https://example.com".into())]);
        run(&mut app, vec![key(Key::Enter, Modifiers::NONE)]);
        let draft = app.draft.as_ref().unwrap();
        assert_eq!(draft.body, "Read more");
        assert_eq!(
            draft.formatting[0].style.link.as_deref(),
            Some("https://example.com/")
        );
        Ok(())
    }

    #[test]
    fn go_chords_navigate_and_refresh_from_the_inbox_list() -> anyhow::Result<()> {
        let path = std::path::PathBuf::from(":memory:");
        let accounts = crate::model::accounts(std::path::Path::new("/tmp"), &path)?;
        let mut store = Store::open(&path)?;
        seed_demo(&mut store, &accounts)?;
        for (account, id) in [("home", "wrong-label"), ("cmu", "nmr-label")] {
            store.put_labels(
                account,
                &[Label {
                    id: id.into(),
                    name: "NMRs".into(),
                    kind: "user".into(),
                }],
            )?;
        }
        let ctx = egui::Context::default();
        let mut app = MailApp::new(
            &ctx,
            store,
            path,
            accounts,
            tokio::runtime::Runtime::new()?,
            true,
        );
        let (sender, mut worker) = tokio::sync::mpsc::unbounded_channel();
        app.workers.insert("cmu".into(), sender);
        let press = |app: &mut MailApp, key| {
            let events = [false, true].map(|pressed| Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: Modifiers::NONE,
            });
            let _ = ctx.run(
                RawInput {
                    events: events.to_vec(),
                    ..Default::default()
                },
                |ctx| app.frame(ctx),
            );
        };
        let chord = |app: &mut MailApp, key| {
            press(app, Key::G);
            let _ = ctx.run(RawInput::default(), |ctx| app.frame(ctx));
            press(app, key);
        };

        chord(&mut app, Key::N);
        assert_eq!(app.scope.as_deref(), Some("cmu"));
        assert_eq!(app.view, View::Tag("nmr-label".into()));
        chord(&mut app, Key::I);
        assert_eq!(app.view, View::Inbox);
        assert!(worker.try_recv().is_err());
        app.open_row(0);
        assert!(app.open.is_some());
        chord(&mut app, Key::I);
        assert!(app.open.is_none());
        assert!(worker.try_recv().is_err());
        press(&mut app, Key::J);
        let focused = app.rows[app.cursor].key.clone();
        chord(&mut app, Key::I);
        assert_eq!(app.rows[app.cursor].key, focused);
        assert!(matches!(worker.try_recv(), Ok(Command::Refresh)));
        Ok(())
    }

    #[test]
    fn archive_shortcut_uses_the_focused_row_and_open_thread() -> anyhow::Result<()> {
        let path = std::path::PathBuf::from(":memory:");
        let accounts = crate::model::accounts(std::path::Path::new("/tmp"), &path)?;
        let mut store = Store::open(&path)?;
        seed_demo(&mut store, &accounts)?;
        let ctx = egui::Context::default();
        let mut app = MailApp::new(
            &ctx,
            store,
            path,
            accounts,
            tokio::runtime::Runtime::new()?,
            true,
        );
        app.demo = false; // Route writes to the test database with no connected Gmail workers.
        let press = |app: &mut MailApp, key, repeat| {
            let event = |pressed, repeat| Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat,
                modifiers: Modifiers::NONE,
            };
            let events = if repeat {
                vec![event(true, true)]
            } else {
                vec![event(false, false), event(true, false)]
            };
            let _ = ctx.run(
                RawInput {
                    events,
                    ..Default::default()
                },
                |context| app.frame(context),
            );
        };
        press(&mut app, Key::J, false);
        let first = app.rows[0].key.clone();
        let focused = app.rows[1].key.clone();
        press(&mut app, Key::E, false);
        assert!(!app.rows.iter().any(|row| row.key == focused));
        assert_eq!(app.rows[0].key, first);
        let count = app.rows.len();
        press(&mut app, Key::E, true);
        assert_eq!(app.rows.len(), count);
        press(&mut app, Key::O, false);
        let open = app.open.clone().unwrap();
        press(&mut app, Key::E, false);
        assert!(app.open.is_none());
        assert!(!app.rows.iter().any(|row| row.key == open));
        Ok(())
    }

    #[test]
    fn reply_editor_follows_the_thread_without_losing_the_draft() -> anyhow::Result<()> {
        let path = std::path::PathBuf::from(":memory:");
        let accounts = crate::model::accounts(std::path::Path::new("/tmp"), &path)?;
        let mut store = Store::open(&path)?;
        seed_demo(&mut store, &accounts)?;
        let ctx = egui::Context::default();
        let mut app = MailApp::new(
            &ctx,
            store,
            path,
            accounts,
            tokio::runtime::Runtime::new()?,
            true,
        );
        app.open = Some(app.rows[0].key.clone());
        app.messages = app.store.thread(app.open.as_ref().unwrap())?;
        app.reply_to(None, 0);
        assert!(app.inline_compose());
        app.draft.as_mut().unwrap().body = "Keep this reply".into();
        let run = |app: &mut MailApp| {
            ctx.run(
                RawInput {
                    screen_rect: Some(Rect::from_min_size(pos2(0., 0.), vec2(1320., 840.))),
                    ..Default::default()
                },
                |ctx| app.frame(ctx),
            )
        };
        let _ = run(&mut app);
        assert!(!app.draft_focus);
        let thread = app.open.take();
        assert!(!app.inline_compose());
        let _ = run(&mut app);
        assert_eq!(app.draft.as_ref().unwrap().body, "Keep this reply");
        app.open = thread;
        assert!(app.inline_compose());
        let _ = run(&mut app);
        assert_eq!(app.draft.as_ref().unwrap().body, "Keep this reply");
        Ok(())
    }

    #[test]
    fn demo_frame_accepts_keyboard_navigation_and_search() -> anyhow::Result<()> {
        let path = std::path::PathBuf::from(":memory:");
        let accounts = crate::model::accounts(std::path::Path::new("/tmp"), &path)?;
        let mut store = Store::open(&path)?;
        seed_demo(&mut store, &accounts)?;
        let ctx = egui::Context::default();
        let mut app = MailApp::new(
            &ctx,
            store,
            path,
            accounts,
            tokio::runtime::Runtime::new()?,
            true,
        );
        let screen = Rect::from_min_size(pos2(0., 0.), vec2(1320., 840.));
        let input = |events| RawInput {
            screen_rect: Some(screen),
            events,
            ..Default::default()
        };
        let key = |key, modifiers| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let run = |app: &mut MailApp, events| ctx.run(input(events), |context| app.frame(context));

        assert!(!run(&mut app, vec![]).shapes.is_empty());
        assert!(app.rows.len() >= 10);
        let _ = run(&mut app, vec![key(Key::J, Modifiers::NONE)]);
        assert_eq!(app.cursor, 1);
        let _ = run(&mut app, vec![key(Key::X, Modifiers::NONE)]);
        assert_eq!(app.selected.len(), 1);
        let _ = run(&mut app, vec![key(Key::O, Modifiers::NONE)]);
        assert_eq!(app.open.as_ref(), Some(&app.rows[1].key));
        let body = app.messages[0].body.clone();
        for width in [820., 1320.] {
            let frame = ctx.run(
                RawInput {
                    screen_rect: Some(Rect::from_min_size(pos2(0., 0.), vec2(width, 680.))),
                    ..Default::default()
                },
                |context| app.frame(context),
            );
            let found = frame
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text().trim() == body.trim() => {
                        Some(text)
                    }
                    _ => None,
                })
                .expect("reader should draw the message body");
            assert!(
                found.visual_bounding_rect().max.x < width - 10.,
                "body extends past the reader"
            );
        }
        let _ = run(&mut app, vec![key(Key::U, Modifiers::NONE)]);
        assert!(app.open.is_none());

        let _ = run(
            &mut app,
            vec![key(Key::Slash, Modifiers::NONE), Event::Text("/".into())],
        );
        assert!(app.query.is_empty());
        let _ = run(
            &mut app,
            vec![Event::Text("calibration OR notebook".into())],
        );
        let _ = run(&mut app, vec![key(Key::Enter, Modifiers::NONE)]);
        assert_eq!(app.view, View::Search("calibration OR notebook".into()));
        assert!(!app.rows.is_empty());

        let _ = run(&mut app, vec![key(Key::K, Modifiers::COMMAND)]);
        assert!(app.modal == Some(Modal::Palette));
        let _ = run(&mut app, vec![Event::Text("inbox".into())]);
        let _ = run(&mut app, vec![key(Key::Enter, Modifiers::NONE)]);
        assert_eq!(app.view, View::Inbox);
        let _ = run(&mut app, vec![key(Key::C, Modifiers::NONE)]);
        assert!(app.draft.is_some());
        Ok(())
    }
}
