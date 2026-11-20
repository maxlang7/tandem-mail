use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

use crate::{
    mime,
    model::{Account, DraftFile, DraftFileSource, Label, RemoteMessage, ThreadKey},
    store::{Download, Scan, State, Store, View, now},
};

#[test]
fn downloads_use_a_new_filename_when_the_original_already_exists() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = mime::save_download(directory.path(), "../../notes.pdf", b"original")?;
    let copy = mime::save_download(directory.path(), "notes.pdf", b"downloaded again")?;
    assert_eq!(path, directory.path().join("notes.pdf"));
    assert_eq!(copy, directory.path().join("notes (1).pdf"));
    assert_eq!(std::fs::read(path)?, b"original");
    assert_eq!(std::fs::read(copy)?, b"downloaded again");
    Ok(())
}

const SAMPLE: &str = concat!(
    "From: Sample Sender <sender@example.com>\r\n",
    "Reply-To: replies@example.com\r\n",
    "To: home@example.com\r\n",
    "Cc: Partner <partner@example.com>\r\n",
    "Subject: Report draft\r\n",
    "Message-ID: <report@example.com>\r\n",
    "References: <previous@example.com>\r\n",
    "MIME-Version: 1.0\r\n",
    "Content-Type: multipart/mixed; boundary=parts\r\n\r\n",
    "--parts\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nSee the calibration worksheet.\r\n",
    "--parts\r\nContent-Type: application/octet-stream\r\n",
    "Content-Disposition: attachment; filename=calibration.csv\r\n",
    "Content-Transfer-Encoding: base64\r\n\r\nYSxiCjEsMgo=\r\n",
    "--parts--\r\n"
);

#[test]
fn downloading_all_keeps_duplicates_and_continues_after_an_unavailable_part() -> anyhow::Result<()>
{
    let raw = SAMPLE.replace(
        "--parts--\r\n",
        "--parts\r\nContent-Type: application/octet-stream\r\n\
         Content-Disposition: attachment; filename=calibration.csv\r\n\
         Content-Transfer-Encoding: base64\r\n\r\nYyxkCjMsNAo=\r\n--parts--\r\n",
    );
    let directory = tempfile::tempdir()?;
    let selection = [
        (0, "calibration.csv".to_owned()),
        (9, "missing.csv".to_owned()),
        (1, "calibration.csv".to_owned()),
    ];
    let results = mime::save_attachments(raw.as_bytes(), directory.path(), &selection)?;
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 2);
    assert!(results[1].is_err());
    assert_eq!(
        std::fs::read(directory.path().join("calibration.csv"))?,
        b"a,b\n1,2\n"
    );
    assert_eq!(
        std::fs::read(directory.path().join("calibration (1).csv"))?,
        b"c,d\n3,4\n"
    );
    Ok(())
}

fn account(id: &str) -> Account {
    Account {
        id: id.into(),
        name: id.into(),
        email: format!("{id}@example.com"),
        credentials: Default::default(),
    }
}

fn thread(account: &str) -> ThreadKey {
    ThreadKey {
        account: account.into(),
        thread: "thread1".into(),
    }
}

fn remote(labels: &[&str], raw: bool) -> RemoteMessage {
    RemoteMessage {
        id: "msg1".into(),
        thread_id: "thread1".into(),
        label_ids: labels.iter().map(ToString::to_string).collect(),
        internal_date: now().to_string(),
        raw: raw.then(|| URL_SAFE_NO_PAD.encode(SAMPLE)),
    }
}

#[test]
fn full_message_and_attachment_search_work_across_accounts() -> anyhow::Result<()> {
    let mut store = Store::open(std::path::Path::new(":memory:"))?;
    store.bind_accounts(&[account("home"), account("cmu")])?;
    store.put_labels(
        "home",
        &[Label {
            id: "Label_9".into(),
            name: "Lab work".into(),
            kind: "user".into(),
        }],
    )?;
    store.put_labels(
        "cmu",
        &[Label {
            id: "Label_9".into(),
            name: "Coursework".into(),
            kind: "user".into(),
        }],
    )?;
    store.apply_messages(
        "home",
        10,
        vec![remote(
            &[
                "INBOX",
                "UNREAD",
                "Label_9",
                "CATEGORY_UPDATES",
                "IMPORTANT",
            ],
            true,
        )],
        &[],
    )?;
    store.apply_messages("cmu", 10, vec![remote(&["TRASH", "Label_9"], true)], &[])?;

    let accounts = ["home".to_owned(), "cmu".to_owned()];
    let expected = [
        (View::Inbox, 1),
        (View::Search("is:important".into()), 1),
        (
            View::Search(
                r#"label:"Lab work" {subject:"report draft" filename:txt} -from:nobody is:unread"#
                    .into(),
            ),
            1,
        ),
        (
            View::Search(
                "has:attachment calibration rfc822msgid:report@example.com cc:partner".into(),
            ),
            1,
        ),
        (
            View::Search("in:anywhere filename:csv has:attachment".into()),
            2,
        ),
        (View::Search("in:trash account:cmu".into()), 1),
        (View::Search("in:archive".into()), 0),
        (
            View::Search("subject:absent OR (subject:report -has:attachment)".into()),
            0,
        ),
    ];
    for (view, count) in expected {
        assert_eq!(
            store.list(&view, &accounts, &[], 20, 0)?.len(),
            count,
            "{view:?}"
        );
    }

    let category = View::Search("in:inbox category:updates".into());
    store.apply_messages("cmu", 10, vec![remote(&["CATEGORY_UPDATES"], false)], &[])?;
    let matching_inbox = store.list(&category, &accounts, &[], 20, 0)?;
    assert_eq!(matching_inbox.len(), 1);
    assert_eq!(matching_inbox[0].key.account, "home");

    let key = thread("home");
    assert_eq!(
        store.download(&key.account, "msg1", &Download::Attachment(0))?,
        b"a,b\n1,2\n"
    );
    assert_eq!(
        store.download(&key.account, "msg1", &Download::Raw)?,
        SAMPLE.as_bytes()
    );
    store.snooze(&key, now() + 60_000)?;
    assert!(store.list(&View::Inbox, &accounts, &[], 20, 0)?.is_empty());
    assert_eq!(store.list(&View::Snoozed, &accounts, &[], 20, 0)?.len(), 1);
    store.snooze(&key, now() - 1)?;
    assert_eq!(store.list(&View::Inbox, &accounts, &[], 20, 0)?.len(), 1);

    let mut wrong = account("home");
    wrong.email = "different@example.com".into();
    assert!(store.bind_accounts(&[wrong]).is_err());
    Ok(())
}

#[test]
fn resync_reuses_raw_cache_and_keeps_removed_mail_as_an_exportable_record() -> anyhow::Result<()> {
    let mut store = Store::open(std::path::Path::new(":memory:"))?;
    let key = thread("home");
    store.defer_reads("home", &["msg1".to_owned()])?;
    assert_eq!(store.pending_count("home")?, 1);
    store.apply_messages("home", 0, vec![remote(&["INBOX", "UNREAD"], true)], &[])?;
    assert_eq!(store.pending_count("home")?, 0);
    let mut state = State {
        history: Some("123".into()),
        scan: Some(Scan {
            epoch: 14,
            next_page: Some("page2".into()),
            inspected: 60,
            total: 300,
        }),
        last_sync: None,
    };
    store.put_state("home", &state)?;
    assert_eq!(
        store.state("home")?.scan.unwrap().next_page.as_deref(),
        Some("page2")
    );
    store.apply_messages("home", 14, vec![remote(&["STARRED"], false)], &[])?;
    state.scan = None;
    store.stale_scan("home", 14, &state)?;
    assert_eq!(store.thread(&key)?[0].labels, ["STARRED"]);
    store.apply_messages("home", 0, vec![], &["msg1".into()])?;
    assert!(store.thread(&key)?.is_empty());
    assert_eq!(
        store.download("home", "msg1", &Download::Raw)?,
        SAMPLE.as_bytes()
    );
    Ok(())
}

#[test]
fn draft_snapshot_round_trips_reply_headers_and_offline_attachments() -> anyhow::Result<()> {
    let mut store = Store::open(std::path::Path::new(":memory:"))?;
    let account = account("home");
    let key = thread(&account.id);
    store.apply_messages("home", 0, vec![remote(&["INBOX"], true)], &[])?;
    let message = store.thread(&key)?.remove(0);
    let mut draft = mime::reply(&message, &account, &key.thread, true);
    assert_eq!(draft.to, "replies@example.com");
    assert!(draft.cc.contains("partner@example.com"));
    assert_eq!(draft.thread.as_deref(), Some("thread1"));
    draft.bcc = "hidden@example.com".into();
    draft.body = "Thanks, here's the copy.".into();
    draft.files.push(DraftFile {
        name: "calibration.csv".into(),
        source: DraftFileSource::Cached {
            account: account.id.clone(),
            message: message.id,
            index: 0,
        },
    });

    let queued = store.queue(&mut draft, &account, now() + 60_000)?;
    assert!(store.drafts()?.is_empty());
    assert!(!store.send_waiting(&account.id)?);
    assert!(store.next_send(&account.id)?.is_none());
    assert_eq!(store.cancel_send(queued)?.unwrap().body, draft.body);

    let queued = store.queue(&mut draft, &account, now() - 1)?;
    let wire = store.next_send(&account.id)?.unwrap();
    assert_eq!(wire.id, queued);
    assert!(store.next_send(&account.id)?.is_none());
    assert!(store.cancel_send(queued)?.is_none());
    let raw = mime::decode(&wire.raw)?;
    let text = String::from_utf8_lossy(&raw);
    assert!(text.contains("In-Reply-To: <report@example.com>"));
    assert!(text.contains("References: <previous@example.com> <report@example.com>"));
    assert!(text.contains("Bcc: hidden@example.com"));
    assert_eq!(mime::attachment(&raw, 0)?, b"a,b\n1,2\n");
    store.recover_send()?;
    assert!(store.next_send(&account.id)?.is_none());
    assert_eq!(store.outbox()?[0].status, "uncertain");
    assert!(store.cancel_send(queued)?.is_some());
    Ok(())
}

#[test]
fn conversation_rows_show_distinct_senders_in_message_order() -> anyhow::Result<()> {
    let mut store = Store::open(std::path::Path::new(":memory:"))?;
    let senders = [
        "Max User <home@example.com>",
        "Lin Z <lin@example.com>",
        "Rani Chen <rani@example.com>",
        "Ruby Nguyen <ruby@example.com>",
        "R. Nguyen <ruby@example.com>",
    ];
    for (i, sender) in senders.iter().enumerate() {
        let mut message = remote(&["INBOX"], true);
        message.id = format!("msg{i}");
        message.internal_date = (i + 1).to_string();
        message.raw = Some(URL_SAFE_NO_PAD.encode(format!(
            "From: {sender}\r\nSubject: Group project\r\n\r\nHello"
        )));
        store.apply_messages("home", 1, vec![message], &[])?;
    }
    let rows = store.list(&View::Inbox, &["home".into()], &[], 20, 0)?;
    let (caption, _) = mime::conversation_senders(&rows[0].senders, "HOME@example.com");
    assert_eq!(caption, "me, Lin, Rani, Ruby");
    assert_eq!(rows[0].count, 5);
    Ok(())
}

#[test]
fn queued_archive_masks_stale_sync_and_keeps_the_thread_in_all_mail() -> anyhow::Result<()> {
    use crate::model::Action;
    let mut store = Store::open(std::path::Path::new(":memory:"))?;
    let home = thread("home");
    let work = thread("cmu");
    for key in [&home, &work] {
        store.apply_messages(
            &key.account,
            1,
            vec![remote(&["INBOX", "UNREAD"], true)],
            &[],
        )?;
    }
    let accounts = ["home".into(), "cmu".into()];
    let archive = Action::labels(&[], &["INBOX"]);
    assert_eq!(
        store.queue_mailbox_changes(std::slice::from_ref(&home), &archive)?,
        1
    );
    assert_eq!(
        store.queue_mailbox_changes(std::slice::from_ref(&home), &archive)?,
        0
    );
    store.apply_messages("home", 1, vec![remote(&["INBOX", "UNREAD"], false)], &[])?;
    assert_eq!(
        store.list(&View::Inbox, &accounts, &[], 20, 0)?[0].key,
        work
    );
    assert_eq!(store.list(&View::All, &accounts, &[], 20, 0)?.len(), 2);
    assert_eq!(
        store.list(&View::Search("in:archive".into()), &accounts, &[], 20, 0)?[0].key,
        home
    );
    assert_eq!(
        store.list(&View::Search("in:inbox".into()), &accounts, &[], 20, 0)?[0].key,
        work
    );

    let update = store.next_mailbox_change("home")?.unwrap();
    store.finish_mailbox_change(&update, false)?;
    assert_eq!(store.list(&View::Inbox, &accounts, &[], 20, 0)?.len(), 2);
    store.queue_mailbox_changes(std::slice::from_ref(&home), &archive)?;
    let update = store.next_mailbox_change("home")?.unwrap();
    let move_to_inbox = Action::labels(&["INBOX"], &[]);
    store.queue_mailbox_changes(std::slice::from_ref(&home), &move_to_inbox)?;
    store.finish_mailbox_change(&update, true)?;
    assert_eq!(store.thread(&home)?[0].labels, ["UNREAD"]);
    assert_eq!(store.list(&View::Inbox, &accounts, &[], 20, 0)?.len(), 2);
    let update = store.next_mailbox_change("home")?.unwrap();
    store.finish_mailbox_change(&update, true)?;
    assert!(store.next_mailbox_change("home")?.is_none());
    assert_eq!(store.list(&View::Inbox, &accounts, &[], 20, 0)?.len(), 2);
    Ok(())
}
