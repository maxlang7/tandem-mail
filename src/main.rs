mod compose_format;
mod gmail;
mod html_mail;
mod luna;
mod mcp;
mod mcp_send;
mod message_body;
mod mime;
mod model;
mod remote_image;
mod search;
mod store;
mod sync;
mod ui;
#[cfg(target_os = "linux")]
mod web_reader;

use std::{
    fs::{DirBuilder, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use eframe::egui;
use store::Store;

fn main() -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is unavailable")?);
    let dir = std::env::var_os("TANDEM_MAIL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share"))
                .join("tandem-mail")
        });
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--bench-render") {
        return tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(html_mail::benchmark::run(&args[1..]));
    }
    if args == ["--check-accounts"] {
        let accounts = model::accounts(&home, &dir)?;
        return tokio::runtime::Runtime::new()?.block_on(check_accounts(accounts));
    }
    const USAGE: &str =
        "Usage: tandem-mail [--demo | --check-accounts | --mcp [--account ID] [--allow-send]]";
    if args.first().is_some_and(|flag| flag == "--mcp") {
        let mut account = None;
        let mut allow_send = false;
        let mut options = args[1..].iter().map(String::as_str);
        while let Some(option) = options.next() {
            match option {
                "--account" if account.is_none() => {
                    account = Some(
                        options
                            .next()
                            .filter(|id| !id.is_empty() && !id.starts_with("--"))
                            .context(USAGE)?,
                    );
                }
                "--allow-send" if !allow_send => allow_send = true,
                _ => anyhow::bail!("{USAGE}"),
            }
        }
        check_cache_directory(&dir).context("Open Tandem Mail first to create its local cache")?;
        let accounts = model::accounts(&home, &dir)?;
        return tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(mcp::serve(
                dir.join("mail.sqlite"),
                accounts,
                account,
                allow_send,
            ));
    }
    let demo = args == ["--demo"];
    anyhow::ensure!(demo || args.is_empty(), "{USAGE}");
    let mut native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1320., 840.])
            .with_min_inner_size([780., 520.])
            .with_app_id("io.tandem_mail.TandemMail"),
        ..Default::default()
    };
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_some() {
        use winit::platform::x11::EventLoopBuilderExtX11;
        native.event_loop_builder = Some(Box::new(|builder| {
            builder.with_x11();
        }));
    }
    eframe::run_native(
        "Tandem Mail",
        native,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let result =
                    prepare_mailbox(&ctx, &home, &dir, demo).map_err(|error| format!("{error:#}"));
                let _ = tx.send(result);
                ctx.request_repaint();
            });
            Ok(Box::new(Desktop {
                state: DesktopState::Loading(rx),
                #[cfg(target_os = "linux")]
                reader: web_reader::Reader::new(cc)
                    .map_err(|error| {
                        eprintln!("HTML reader: {error:#}");
                    })
                    .ok(),
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("Could not open Tandem Mail: {e}"))
}

struct ReadyMailbox {
    app: ui::MailApp,
    _lock: Option<File>,
}

enum DesktopState {
    Loading(std::sync::mpsc::Receiver<Result<ReadyMailbox, String>>),
    Ready(Box<ReadyMailbox>),
    Failed(String),
}

struct Desktop {
    state: DesktopState,
    #[cfg(target_os = "linux")]
    reader: Option<web_reader::Reader>,
}

impl eframe::App for Desktop {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        use std::sync::mpsc::TryRecvError;
        if let DesktopState::Loading(receiver) = &self.state {
            match receiver.try_recv() {
                Ok(Ok(mailbox)) => self.state = DesktopState::Ready(Box::new(mailbox)),
                Ok(Err(error)) => self.state = DesktopState::Failed(error),
                Err(TryRecvError::Disconnected) => {
                    self.state = DesktopState::Failed("The mailbox loader stopped".into());
                }
                Err(TryRecvError::Empty) => {}
            }
        }

        if let DesktopState::Ready(mailbox) = &mut self.state {
            #[cfg(target_os = "linux")]
            if let Some(reader) = &mut self.reader {
                for url in reader.begin_frame(ctx) {
                    mailbox.app.open_mail_link(&url);
                }
            }
            mailbox.app.update(ctx, frame);
            #[cfg(target_os = "linux")]
            if let Some(reader) = &mut self.reader {
                reader.end_frame(
                    ctx,
                    mailbox.app.reader_is_covered() || egui::Popup::is_any_open(ctx),
                );
            }
            return;
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::from_rgb(29, 32, 33)))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.38);
                    ui.label(
                        egui::RichText::new("Tandem Mail")
                            .size(24.)
                            .color(egui::Color32::from_rgb(235, 219, 178)),
                    );
                    let text = match &self.state {
                        DesktopState::Failed(error) => error.as_str(),
                        _ => "Opening mailbox…",
                    };
                    ui.label(
                        egui::RichText::new(text).color(egui::Color32::from_rgb(189, 174, 147)),
                    );
                });
            });
    }
    #[cfg(target_os = "linux")]
    fn on_exit(&mut self, _: Option<&eframe::glow::Context>) {
        self.reader = None;
    }
}

fn prepare_mailbox(
    ctx: &egui::Context,
    home: &Path,
    dir: &Path,
    demo: bool,
) -> Result<ReadyMailbox> {
    let runtime = tokio::runtime::Runtime::new()?;
    let accounts = if demo {
        model::accounts(home, Path::new(":memory:"))?
    } else {
        model::accounts(home, dir)?
    };
    let (path, lock) = if demo {
        (PathBuf::from(":memory:"), None)
    } else {
        let (path, lock) = private_cache(dir)?;
        (path, Some(lock))
    };
    let mut store = Store::open(&path)?;
    store.bind_accounts(&accounts)?;
    store.recover_send()?;
    if demo {
        ui::seed_demo(&mut store, &accounts)?;
    }
    let app = ui::MailApp::new(ctx, store, path, accounts, runtime, demo);
    Ok(ReadyMailbox { app, _lock: lock })
}

fn private_cache(dir: &Path) -> Result<(PathBuf, File)> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    check_cache_directory(dir)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join("app.lock"))?;
    lock.try_lock().map_err(|e| {
        anyhow::anyhow!("Could not lock the mail cache. Close any other Tandem Mail window. {e}")
    })?;
    let path = dir.join("mail.sqlite");
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)?;
    Ok((path, lock))
}

fn check_cache_directory(dir: &Path) -> Result<()> {
    let metadata = dir.symlink_metadata()?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "Choose an owner-only directory (0700) for the mail cache: {}",
        dir.display()
    );
    Ok(())
}

async fn check_accounts(accounts: Vec<model::Account>) -> Result<()> {
    let outcomes = futures::future::join_all(accounts.into_iter().map(|account| async move {
        let check = async {
            let gmail = gmail::Gmail::new(account.clone())?;
            let profile = gmail.profile().await?;
            let (labels, filters, listing) = tokio::try_join!(
                gmail.label_list(),
                gmail.filters(),
                gmail.message_list(None, None)
            )?;
            if let Some(first) = listing.messages.first() {
                let raw = gmail
                    .message(&first.id, true)
                    .await?
                    .raw
                    .context("Gmail omitted the raw message")?;
                mime::decode(&raw)?;
            }
            Ok::<_, anyhow::Error>((profile.messages_total, labels.len(), filters.len()))
        }
        .await;
        (account.email, check)
    }))
    .await;
    let mut failed = false;
    for (email, result) in outcomes {
        match result {
            Ok((messages, labels, filters)) => println!(
                "{email}: verified, {messages} messages, {labels} labels, {filters} filters"
            ),
            Err(error) => {
                failed = true;
                eprintln!("{email}: {error}");
            }
        }
    }
    anyhow::ensure!(!failed, "One or more Gmail checks failed");
    Ok(())
}

#[cfg(test)]
mod regressions;
