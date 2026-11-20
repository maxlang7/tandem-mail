use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::{StreamExt, stream};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::{
    gmail::{self, Gmail, RemoteId},
    model::{Account, Action, Filter, ThreadKey},
    store::{self, Scan, State, Store},
};

const POLL: Duration = Duration::from_secs(300);
const RETRY: Duration = Duration::from_secs(25);
const CHUNK: usize = 6;

#[derive(Debug)]
pub enum Command {
    Refresh,
    Change {
        keys: Vec<ThreadKey>,
        action: Action,
    },
    Search {
        generation: u64,
        query: String,
        cursor: Option<String>,
    },
    CreateLabel(String),
    ReadFilters,
    CreateFilter(Filter),
    DeleteFilter(String),
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub message: String,
    pub failed: bool,
    pub last_sync: Option<i64>,
}

#[derive(Debug)]
pub enum Event {
    Status(String, Status),
    Changed,
    Feedback(String, bool),
    Search {
        account: String,
        generation: u64,
        threads: Vec<ThreadKey>,
        cursor: Option<String>,
        error: Option<String>,
    },
}

pub type Commands = mpsc::UnboundedSender<Command>;

pub fn launch(
    runtime: &tokio::runtime::Runtime,
    path: PathBuf,
    account: Account,
    events: std::sync::mpsc::Sender<Event>,
    ctx: eframe::egui::Context,
) -> Commands {
    let (tx, rx) = mpsc::unbounded_channel();
    runtime.spawn(async move {
        let id = account.id.clone();
        let display = account.name.clone();
        let result = async {
            let store = Store::open(&path)?;
            let state = store.state(&id)?;
            let mut worker = Worker {
                gmail: Gmail::new(account)?,
                store,
                id: id.clone(),
                display,
                state,
                next_mailbox_change: Instant::now(),
                events: events.clone(),
                ctx: ctx.clone(),
            };
            worker.run(rx).await;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            let _ = events.send(Event::Status(
                id,
                Status {
                    message: error.to_string(),
                    failed: true,
                    last_sync: None,
                },
            ));
            ctx.request_repaint();
        }
    });
    tx
}

struct Worker {
    gmail: Gmail,
    store: Store,
    id: String,
    display: String,
    state: State,
    next_mailbox_change: Instant,
    events: std::sync::mpsc::Sender<Event>,
    ctx: eframe::egui::Context,
}

impl Worker {
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
        self.ctx.request_repaint();
    }

    fn status(&self, message: String, failed: bool) {
        if failed {
            eprintln!("{} sync: {message}", self.display);
        }
        self.emit(Event::Status(
            self.id.clone(),
            Status {
                message,
                failed,
                last_sync: self.state.last_sync,
            },
        ));
    }

    async fn run(&mut self, mut rx: mpsc::UnboundedReceiver<Command>) {
        let mut next_poll = Instant::now();
        let mut next_scan = next_poll;
        let mut next_send = next_poll;
        loop {
            self.process_mailbox_queue().await;
            if let Ok(command) = rx.try_recv() {
                if self.handle(command).await {
                    next_poll = Instant::now();
                    next_scan = next_poll;
                }
                continue;
            }
            if Instant::now() >= next_send {
                match self.send_due().await {
                    Ok(()) => next_send = Instant::now() + Duration::from_secs(1),
                    Err(error) => {
                        self.emit(Event::Feedback(
                            format!("{} outbox waiting: {error}", self.display),
                            true,
                        ));
                        next_send = Instant::now() + RETRY;
                    }
                }
            }
            if Instant::now() >= next_poll {
                match self.refresh().await {
                    Ok(()) => {
                        next_poll = Instant::now() + POLL;
                        next_scan = Instant::now();
                        self.show_progress();
                    }
                    Err(error) => {
                        self.status(format!("{error:#}"), true);
                        next_poll = Instant::now() + retry_after(&error);
                        next_scan = next_poll;
                    }
                }
            }
            if self.state.scan.is_some() && Instant::now() >= next_scan {
                match self.scan_page().await {
                    Ok(()) => self.show_progress(),
                    Err(error) => {
                        self.status(format!("Download paused: {error:#}"), true);
                        next_scan = Instant::now() + retry_after(&error);
                    }
                }
                continue;
            }
            let wake = if self.state.scan.is_some() {
                next_poll
                    .min(next_scan)
                    .min(next_send)
                    .min(self.next_mailbox_change)
            } else {
                next_poll.min(next_send).min(self.next_mailbox_change)
            };
            tokio::select! {
                message = rx.recv() => match message {
                    Some(command) => if self.handle(command).await {
                        next_poll = Instant::now();
                        next_scan = next_poll;
                    },
                    None => return,
                },
                _ = tokio::time::sleep_until(wake) => {}
            }
        }
    }

    fn show_progress(&self) {
        let info = self.store.pending_count(&self.id).and_then(|pending| {
            let suffix = if pending > 0 {
                format!(" · {pending} retrying")
            } else {
                String::new()
            };
            if let Some(scan) = &self.state.scan {
                Ok(format!(
                    "Caching {} / {} messages{suffix}",
                    scan.inspected,
                    scan.total.max(scan.inspected as u64)
                ))
            } else {
                let count = self.store.present_count(&self.id)?;
                Ok(format!("{count} messages cached{suffix}"))
            }
        });
        match info {
            Ok(message) => self.status(message, false),
            Err(error) => self.status(error.to_string(), true),
        }
    }

    async fn refresh(&mut self) -> Result<()> {
        let labels = self
            .gmail
            .label_list()
            .await
            .context("Could not sync labels")?;
        self.store.put_labels(&self.id, &labels)?;
        self.emit(Event::Changed);
        match self.state.history.clone() {
            Some(cursor) => match self.history(&cursor).await {
                Ok(()) => {}
                Err(error) if gmail::missing(&error) => self.start_scan().await?,
                Err(error) => return Err(error),
            },
            None => self.start_scan().await?,
        }
        let retry = self.store.pending_reads(&self.id)?;
        if let Err(error) = self
            .fetch_messages(retry.into_iter().map(|id| RemoteId { id }).collect(), true)
            .await
        {
            eprintln!("{} deferred downloads: {error:#}", self.display);
        }
        Ok(())
    }

    async fn start_scan(&mut self) -> Result<()> {
        let profile = self.gmail.profile().await?;
        let state = State {
            history: Some(profile.history_id),
            scan: Some(Scan {
                epoch: store::now(),
                next_page: None,
                inspected: 0,
                total: profile.messages_total,
            }),
            last_sync: self.state.last_sync,
        };
        self.store.put_state(&self.id, &state)?;
        self.state = state;
        Ok(())
    }

    fn epoch(&self) -> i64 {
        self.state.scan.as_ref().map_or(0, |scan| scan.epoch)
    }

    async fn fetch(&mut self, messages: Vec<RemoteId>) -> Result<Vec<ThreadKey>> {
        self.fetch_messages(messages, false).await
    }

    async fn fetch_messages(
        &mut self,
        messages: Vec<RemoteId>,
        defer: bool,
    ) -> Result<Vec<ThreadKey>> {
        let mut output = Vec::with_capacity(messages.len());
        for chunk in messages.chunks(CHUNK) {
            self.process_mailbox_queue().await;
            let mut requested = Vec::with_capacity(chunk.len());
            for item in chunk {
                requested.push((item.id.clone(), !self.store.has_raw(&self.id, &item.id)?));
            }
            let gmail = &self.gmail;
            let downloads = stream::iter(requested.into_iter().map(|(id, raw)| async move {
                let result = gmail.message(&id, raw).await;
                (id, result)
            }))
            .buffer_unordered(CHUNK)
            .collect::<Vec<_>>()
            .await;
            let mut updated = Vec::new();
            let mut gone = Vec::new();
            let mut failures = Vec::new();
            for (id, result) in downloads {
                match result {
                    Ok(message) => {
                        output.push(ThreadKey {
                            account: self.id.clone(),
                            thread: message.thread_id.clone(),
                        });
                        updated.push(message);
                    }
                    Err(error) if gmail::missing(&error) => gone.push(id),
                    Err(error) => failures.push((id, error)),
                }
            }
            self.store
                .apply_messages(&self.id, self.epoch(), updated, &gone)?;
            if let Some(index) = failures
                .iter()
                .position(|(_, error)| gmail::rate_limited(error))
            {
                self.emit(Event::Changed);
                return Err(failures.swap_remove(index).1);
            }
            if !failures.is_empty() {
                let retry_later = defer && (chunk.len() == 1 || failures.len() < chunk.len());
                if retry_later {
                    let ids = failures
                        .iter()
                        .map(|(id, _)| id.clone())
                        .collect::<Vec<_>>();
                    self.store.defer_reads(&self.id, &ids)?;
                    eprintln!(
                        "{} deferred {} message(s): {:#}",
                        self.display,
                        ids.len(),
                        failures[0].1
                    );
                } else {
                    self.emit(Event::Changed);
                    return Err(failures.remove(0).1);
                }
            }
        }
        if !messages.is_empty() {
            self.emit(Event::Changed);
        }
        Ok(output)
    }

    async fn scan_page(&mut self) -> Result<()> {
        let mut scan = self.state.scan.clone().context("No pending scan")?;
        let listing = match self
            .gmail
            .message_list(scan.next_page.as_deref(), None)
            .await
        {
            Ok(listing) => listing,
            Err(error) if scan.next_page.is_some() && gmail::bad_request(&error) => {
                self.start_scan().await?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let inspected = listing.messages.len();
        let mut remaining = Vec::with_capacity(inspected);
        for message in listing.messages {
            if !self.store.scanned(&self.id, &message.id, scan.epoch)? {
                remaining.push(message);
            }
        }
        self.fetch_messages(remaining, true).await?;
        if let Some(page) = listing.next_page_token {
            scan.inspected += inspected;
            scan.next_page = Some(page);
            let state = State {
                scan: Some(scan),
                ..self.state.clone()
            };
            self.store.put_state(&self.id, &state)?;
            self.state = state;
        } else {
            // Catch changes since the profile snapshot before retiring messages absent from the scan.
            if let Some(cursor) = self.state.history.clone() {
                match self.history(&cursor).await {
                    Ok(()) => {}
                    Err(error) if gmail::missing(&error) => return self.start_scan().await,
                    Err(error) => return Err(error),
                }
            }
            let state = State {
                scan: None,
                last_sync: Some(store::now()),
                ..self.state.clone()
            };
            self.store.stale_scan(&self.id, scan.epoch, &state)?;
            self.state = state;
            self.emit(Event::Changed);
        }
        Ok(())
    }

    async fn history(&mut self, start: &str) -> Result<()> {
        let mut token = None;
        loop {
            let page = self.gmail.history(start, token.as_deref()).await?;
            let mut seen = HashSet::new();
            let ids = page
                .history
                .into_iter()
                .flat_map(|event| {
                    event.messages.into_iter().chain(
                        event
                            .messages_added
                            .into_iter()
                            .chain(event.messages_deleted)
                            .chain(event.labels_added)
                            .chain(event.labels_removed)
                            .map(|entry| entry.message),
                    )
                })
                .filter(|entry| seen.insert(entry.id.clone()))
                .collect();
            self.fetch_messages(ids, true).await?;
            token = page.next_page_token;
            if token.is_none() {
                let state = State {
                    history: Some(page.history_id),
                    last_sync: Some(store::now()),
                    ..self.state.clone()
                };
                self.store.put_state(&self.id, &state)?;
                self.state = state;
                return Ok(());
            }
        }
    }

    async fn handle(&mut self, command: Command) -> bool {
        let result = match command {
            Command::Refresh => return true,
            Command::Change { keys, action } => self.change(keys, &action).await,
            Command::Search {
                generation,
                query,
                cursor,
            } => {
                let result = async {
                    let listing = self
                        .gmail
                        .message_list(cursor.as_deref(), Some(&query))
                        .await?;
                    Ok::<_, anyhow::Error>((
                        self.fetch(listing.messages).await?,
                        listing.next_page_token,
                    ))
                }
                .await;
                let (threads, cursor, error) = match result {
                    Ok((threads, cursor)) => (threads, cursor, None),
                    Err(error) => (vec![], None, Some(error.to_string())),
                };
                self.emit(Event::Search {
                    account: self.id.clone(),
                    generation,
                    threads,
                    cursor,
                    error,
                });
                return false;
            }
            Command::CreateLabel(name) => {
                async {
                    let label = self.gmail.create_label(&name).await?;
                    let mut labels = self
                        .store
                        .labels()?
                        .into_iter()
                        .filter_map(|(account, label)| (account == self.id).then_some(label))
                        .collect::<Vec<_>>();
                    labels.push(label);
                    self.store.put_labels(&self.id, &labels)?;
                    self.emit(Event::Changed);
                    Ok(Some("Label created".into()))
                }
                .await
            }
            Command::ReadFilters => self.read_filters().await.map(|_| None),
            Command::CreateFilter(filter) => {
                async {
                    self.gmail.create_filter(&filter).await?;
                    let refresh = self.read_filters().await;
                    Ok(Some(match refresh {
                        Ok(()) => "Gmail filter created".into(),
                        Err(error) => format!("Filter created. Could not reload the list: {error}"),
                    }))
                }
                .await
            }
            Command::DeleteFilter(id) => {
                async {
                    self.gmail.delete_filter(&id).await?;
                    let refresh = self.read_filters().await;
                    Ok(Some(match refresh {
                        Ok(()) => "Gmail filter deleted".into(),
                        Err(error) => format!("Filter deleted. Could not reload the list: {error}"),
                    }))
                }
                .await
            }
        };
        match result {
            Ok(Some(message)) => self.emit(Event::Feedback(
                format!("{} · {message}", self.display),
                false,
            )),
            Ok(None) => {}
            Err(error) => self.emit(Event::Feedback(format!("{} · {error}", self.display), true)),
        }
        false
    }

    async fn process_mailbox_queue(&mut self) {
        if Instant::now() < self.next_mailbox_change {
            return;
        }
        match self.apply_next_mailbox_change().await {
            Ok(true) => self.next_mailbox_change = Instant::now(),
            Ok(false) => self.next_mailbox_change = Instant::now() + Duration::from_secs(1),
            Err(error) => {
                self.next_mailbox_change = Instant::now() + retry_after(&error);
                self.emit(Event::Feedback(
                    format!("{} · Mailbox change queued. {error}", self.display),
                    true,
                ));
            }
        }
    }

    async fn apply_next_mailbox_change(&mut self) -> Result<bool> {
        let Some(change) = self.store.next_mailbox_change(&self.id)? else {
            return Ok(false);
        };
        let outcome = self
            .gmail
            .change_thread(&change.key.thread, &change.action)
            .await;
        match outcome {
            Ok(messages) => {
                if change.action.inbox_override().is_none() {
                    let mut known = Vec::new();
                    for message in messages {
                        if self.store.has_raw(&self.id, &message.id)? {
                            known.push(message);
                        }
                    }
                    self.store
                        .apply_messages(&self.id, self.epoch(), known, &[])?;
                }
                self.store.finish_mailbox_change(&change, true)?;
                let status = match change.action.inbox_override() {
                    Some(false) => "Archived",
                    Some(true) => "Moved to Inbox",
                    None => "Mailbox updated",
                };
                self.emit(Event::Feedback(
                    format!("{} · {status}", self.display),
                    false,
                ));
            }
            Err(error) if gmail::retryable_change(&error) => return Err(error),
            Err(error) => {
                if gmail::missing(&error) {
                    self.store.mark_deleted_thread(&change.key)?;
                }
                self.store.finish_mailbox_change(&change, false)?;
                self.emit(Event::Feedback(
                    format!("{} · Mailbox change failed. {error}", self.display),
                    true,
                ));
            }
        }
        Ok(true)
    }

    async fn change(&mut self, keys: Vec<ThreadKey>, action: &Action) -> Result<Option<String>> {
        let mut completed = 0;
        let total = keys.len();
        let mut failures = Vec::new();
        for key in keys {
            match self.gmail.change_thread(&key.thread, action).await {
                Ok(messages) => {
                    let mut known = vec![];
                    let mut unknown = vec![];
                    for message in messages {
                        if self.store.has_raw(&self.id, &message.id)? {
                            known.push(message);
                        } else {
                            unknown.push(RemoteId { id: message.id });
                        }
                    }
                    self.store
                        .apply_messages(&self.id, self.epoch(), known, &[])?;
                    if let Err(error) = self.fetch(unknown).await {
                        failures.push(format!(
                            "Gmail updated the thread. Cache refresh failed: {error}"
                        ));
                    }
                    completed += 1;
                }
                Err(error) if gmail::missing(&error) => {
                    self.store.mark_deleted_thread(&key)?;
                    failures.push("Thread no longer exists in Gmail".into());
                }
                Err(error) => failures.push(error.to_string()),
            }
        }
        self.emit(Event::Changed);
        if let Some(first) = failures.first() {
            anyhow::bail!("Updated {completed} of {total}. {first}");
        }
        Ok(Some(format!(
            "Updated {completed} {}",
            if completed == 1 { "thread" } else { "threads" }
        )))
    }

    async fn read_filters(&mut self) -> Result<()> {
        let filters = self.gmail.filters().await?;
        self.store.put_filters(&self.id, &filters)?;
        self.emit(Event::Changed);
        Ok(())
    }

    async fn send_due(&mut self) -> Result<()> {
        if !self.store.send_waiting(&self.id)? {
            return Ok(());
        }
        self.gmail.ready_to_send().await?;
        let Some(mail) = self.store.next_send(&self.id)? else {
            return Ok(());
        };
        match self.gmail.send(&mail.raw, mail.thread.as_deref()).await {
            Ok(message) => {
                self.store.complete_send(mail.id, None)?;
                self.emit(Event::Feedback(
                    format!("{} · Message sent", self.display),
                    false,
                ));
                if let Err(error) = self.fetch(vec![message]).await {
                    self.emit(Event::Feedback(
                        format!("Message sent. Cache refresh failed: {error}"),
                        true,
                    ));
                }
            }
            Err(error) => {
                self.store.complete_send(
                    mail.id,
                    Some(&format!("{error}. Check Sent before retrying.")),
                )?;
                self.emit(Event::Feedback(
                    format!("{} · Check the outbox", self.display),
                    true,
                ));
            }
        }
        self.emit(Event::Changed);
        Ok(())
    }
}

fn retry_after(error: &anyhow::Error) -> Duration {
    if gmail::rate_limited(error) {
        gmail::QUOTA_PAUSE
    } else {
        RETRY
    }
}
