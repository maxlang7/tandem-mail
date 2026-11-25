use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, OptionalExtension, TransactionBehavior, params, params_from_iter, types::Value,
};
use serde::{Deserialize, Serialize};

use crate::{
    mime,
    model::{
        Account, Action, Draft, DraftFile, DraftFileSource, Filter, Label, Message, Outgoing,
        RemoteMessage, Row, ThreadKey,
    },
    search,
};

pub struct Store(Connection);

pub struct ContactHeaders {
    pub sender: String,
    pub recipients: String,
    pub cc: String,
    pub sent: bool,
}

pub struct MailExcerpt {
    pub sender: String,
    pub recipients: String,
    pub subject: String,
    pub body: String,
    pub date: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Scan {
    pub epoch: i64,
    pub next_page: Option<String>,
    pub total: u64,
    pub inspected: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct State {
    pub history: Option<String>,
    pub scan: Option<Scan>,
    pub last_sync: Option<i64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum View {
    #[default]
    Inbox,
    Tag(String),
    All,
    Snoozed,
    Drafts,
    Outbox,
    Search(String),
    Online,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BodyView {
    #[default]
    Auto,
    Original,
    Selectable,
}

#[derive(Clone, Debug)]
pub struct SavedSearch {
    pub id: i64,
    pub name: String,
    pub query: String,
    pub account: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Download {
    Raw,
    Attachment(usize),
}

pub struct MailboxChange {
    pub id: i64,
    pub key: ThreadKey,
    pub action: Action,
}

// This predicate expects a messages row named m. Pending changes take precedence over synced tags.
pub const IN_INBOX: &str = "coalesce(
    (SELECT q.inbox FROM mailbox_changes q WHERE q.account=m.account AND q.thread=m.thread
       AND q.inbox IS NOT NULL ORDER BY q.id DESC LIMIT 1),
    EXISTS (SELECT 1 FROM tags WHERE msg=m.pk AND label='INBOX')
)";

const MESSAGE_COLUMNS: &str =
    "id,sender,recipients,cc,bcc,reply_to,subject,body,date,rfc_id,refs,files,
    coalesce((SELECT json_group_array(label) FROM tags WHERE msg=m.pk),'[]')";

pub struct QueuedMail {
    pub id: i64,
    pub raw: String,
    pub thread: Option<String>,
}

impl Store {
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_secs(8))?;
        Ok(Self(connection))
    }

    pub fn open_cache_writer(path: &Path) -> Result<Self> {
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(8))?;
        Ok(Self(connection))
    }

    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(8))?;
        connection.execute_batch("
            PRAGMA journal_mode=WAL;
            PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS identities (id TEXT PRIMARY KEY, email TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS states (account TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS messages (
              pk INTEGER PRIMARY KEY, account TEXT NOT NULL, id TEXT NOT NULL, thread TEXT NOT NULL,
              raw BLOB NOT NULL, date INTEGER NOT NULL, size INTEGER NOT NULL, present INTEGER NOT NULL, epoch INTEGER NOT NULL,
              sender TEXT NOT NULL, recipients TEXT NOT NULL, cc TEXT NOT NULL, bcc TEXT NOT NULL, reply_to TEXT NOT NULL,
              rfc_id TEXT NOT NULL, refs TEXT NOT NULL, subject TEXT NOT NULL, body TEXT NOT NULL, snippet TEXT NOT NULL,
              files TEXT NOT NULL, has_files INTEGER NOT NULL,
              UNIQUE (account,id)
            );
            CREATE INDEX IF NOT EXISTS messages_thread ON messages(account,thread,present,date);
            CREATE INDEX IF NOT EXISTS messages_recent ON messages(present,date DESC);
            CREATE TABLE IF NOT EXISTS pending_reads (
              account TEXT NOT NULL, id TEXT NOT NULL, since INTEGER NOT NULL,
              PRIMARY KEY(account,id)
            );
            CREATE TABLE IF NOT EXISTS labels (account TEXT NOT NULL, id TEXT NOT NULL, name TEXT NOT NULL,
              kind TEXT NOT NULL, PRIMARY KEY(account,id));
            CREATE TABLE IF NOT EXISTS tags (msg INTEGER NOT NULL REFERENCES messages(pk), label TEXT NOT NULL,
              PRIMARY KEY(msg,label));
            CREATE INDEX IF NOT EXISTS tags_label ON tags(label,msg);
            CREATE VIRTUAL TABLE IF NOT EXISTS mail_fts USING fts5(
              subject, body, sender, recipients, files, tokenize='unicode61 remove_diacritics 2'
            );
            CREATE TABLE IF NOT EXISTS remote_images (url TEXT PRIMARY KEY, data BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS mailbox_changes (
              id INTEGER PRIMARY KEY, account TEXT NOT NULL, thread TEXT NOT NULL,
              action TEXT NOT NULL, inbox INTEGER
            );
            CREATE INDEX IF NOT EXISTS mailbox_changes_account ON mailbox_changes(account,id);
            CREATE INDEX IF NOT EXISTS mailbox_changes_thread ON mailbox_changes(account,thread,id);
            CREATE TABLE IF NOT EXISTS settings (id TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS filters (account TEXT PRIMARY KEY, rules TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS snoozes (
              account TEXT NOT NULL, thread TEXT NOT NULL, due INTEGER NOT NULL, PRIMARY KEY (account,thread)
            );
            CREATE TABLE IF NOT EXISTS drafts (id INTEGER PRIMARY KEY, account TEXT NOT NULL, data TEXT NOT NULL,
              modified INTEGER NOT NULL, state TEXT NOT NULL DEFAULT 'draft');
            CREATE TABLE IF NOT EXISTS draft_files (id INTEGER PRIMARY KEY, data BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS outbox (
              id INTEGER PRIMARY KEY, account TEXT NOT NULL, draft INTEGER NOT NULL REFERENCES drafts(id),
              subject TEXT NOT NULL, raw TEXT NOT NULL, thread TEXT, due INTEGER NOT NULL,
              status TEXT NOT NULL DEFAULT 'queued', note TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS outbox_waiting ON outbox(account,status,due);
            CREATE TABLE IF NOT EXISTS saved_searches (
              id INTEGER PRIMARY KEY, name TEXT NOT NULL, query TEXT NOT NULL, account TEXT, active INTEGER NOT NULL DEFAULT 1
            );
        ")?;
        Ok(Self(connection))
    }

    pub fn bind_accounts(&mut self, accounts: &[Account]) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for account in accounts {
            tx.execute(
                "INSERT OR IGNORE INTO identities VALUES (?1,?2)",
                params![account.id, account.email],
            )?;
            let registered: String = tx.query_row(
                "SELECT email FROM identities WHERE id=?1",
                [&account.id],
                |r| r.get(0),
            )?;
            ensure!(
                registered.eq_ignore_ascii_case(&account.email),
                "Account ID '{}' already belongs to {} in this cache. Use a different ID for {}.",
                account.id,
                registered,
                account.email
            );
        }
        tx.commit()?;
        Ok(())
    }

    pub fn cached_identity(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row("SELECT email FROM identities WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn state(&self, account: &str) -> Result<State> {
        let value: Option<String> = self
            .0
            .query_row(
                "SELECT value FROM states WHERE account=?1",
                [account],
                |r| r.get(0),
            )
            .optional()?;
        Ok(value
            .map(|value| serde_json::from_str(&value))
            .transpose()?
            .unwrap_or_default())
    }

    pub fn put_state(&self, account: &str, state: &State) -> Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO states VALUES (?1,?2)",
            params![account, serde_json::to_string(state)?],
        )?;
        Ok(())
    }

    pub fn stale_scan(&mut self, account: &str, epoch: i64, state: &State) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE messages SET present=0 WHERE account=?1 AND epoch<>?2",
            params![account, epoch],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO states VALUES (?1,?2)",
            params![account, serde_json::to_string(state)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn present_count(&self, account: &str) -> Result<usize> {
        Ok(self.0.query_row(
            "SELECT count(*) FROM messages WHERE account=?1 AND present=1",
            [account],
            |r| r.get(0),
        )?)
    }

    pub fn scanned(&self, account: &str, id: &str, epoch: i64) -> Result<bool> {
        Ok(self.0.query_row(
            "SELECT EXISTS (SELECT 1 FROM messages WHERE account=?1 AND id=?2 AND epoch=?3 AND present=1)",
            params![account, id, epoch],
            |row| row.get(0),
        )?)
    }

    pub fn has_raw(&self, account: &str, id: &str) -> Result<bool> {
        Ok(self.0.query_row(
            "SELECT EXISTS (SELECT 1 FROM messages WHERE account=?1 AND id=?2)",
            params![account, id],
            |row| row.get(0),
        )?)
    }

    pub fn apply_messages(
        &mut self,
        account: &str,
        epoch: i64,
        messages: Vec<RemoteMessage>,
        gone: &[String],
    ) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for id in gone {
            tx.execute(
                "UPDATE messages SET present=0 WHERE account=?1 AND id=?2",
                params![account, id],
            )?;
        }
        for id in gone {
            tx.execute(
                "DELETE FROM pending_reads WHERE account=?1 AND id=?2",
                params![account, id],
            )?;
        }
        for message in messages {
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT pk FROM messages WHERE account=?1 AND id=?2",
                    params![account, message.id],
                    |r| r.get(0),
                )
                .optional()?;
            let pk = if let Some(raw) = message.raw.as_deref() {
                let raw = mime::decode(raw)?;
                let parsed = mime::inspect(&raw);
                let snippet: String = parsed
                    .body
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(210)
                    .collect();
                let filenames = parsed
                    .files
                    .iter()
                    .map(|(_, name)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                let date = message.internal_date.parse::<i64>().unwrap_or_default();
                let files = serde_json::to_string(&parsed.files)?;
                let pk: i64 = tx.query_row("
                    INSERT INTO messages (account,id,thread,raw,date,size,present,epoch,sender,recipients,cc,bcc,reply_to,rfc_id,refs,subject,body,snippet,files,has_files)
                    VALUES (?1,?2,?3,?4,?5,?6,1,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
                    ON CONFLICT(account,id) DO UPDATE SET thread=excluded.thread, raw=excluded.raw,date=excluded.date,
                    size=excluded.size,present=1,epoch=excluded.epoch,sender=excluded.sender,recipients=excluded.recipients,
                    cc=excluded.cc,bcc=excluded.bcc,reply_to=excluded.reply_to,rfc_id=excluded.rfc_id,refs=excluded.refs,
                    subject=excluded.subject,body=excluded.body,snippet=excluded.snippet,files=excluded.files,has_files=excluded.has_files
                    RETURNING pk",
                    params![account, message.id, message.thread_id, raw, date, raw.len(), epoch,
                        parsed.sender, parsed.recipients, parsed.cc, parsed.bcc, parsed.reply_to, parsed.message_id,
                        parsed.references, parsed.subject, parsed.body, snippet, files, !parsed.files.is_empty()], |r| r.get(0))?;
                tx.execute("DELETE FROM mail_fts WHERE rowid=?1", [pk])?;
                tx.execute("INSERT INTO mail_fts (rowid,subject,body,sender,recipients,files) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![pk, parsed.subject, parsed.body, parsed.sender, format!("{} {} {}", parsed.recipients, parsed.cc, parsed.bcc), filenames])?;
                pk
            } else {
                let pk = existing.context("Message body was missing from Gmail")?;
                tx.execute(
                    "UPDATE messages SET present=1,epoch=?2,thread=?3 WHERE pk=?1",
                    params![pk, epoch, message.thread_id],
                )?;
                pk
            };
            tx.execute(
                "DELETE FROM pending_reads WHERE account=?1 AND id=?2",
                params![account, message.id],
            )?;
            tx.execute("DELETE FROM tags WHERE msg=?1", [pk])?;
            for label in &message.label_ids {
                tx.execute(
                    "INSERT OR IGNORE INTO tags VALUES (?1,?2)",
                    params![pk, label],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn defer_reads(&mut self, account: &str, ids: &[String]) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for id in ids {
            tx.execute(
                "INSERT OR REPLACE INTO pending_reads VALUES (?1,?2,?3)",
                params![account, id, now()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn pending_reads(&self, account: &str) -> Result<Vec<String>> {
        let mut query = self.0.prepare(
            "SELECT id FROM pending_reads WHERE account=?1 AND since<?2 ORDER BY since LIMIT 8",
        )?;
        let rows = query.query_map(params![account, now() - 240_000], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn pending_count(&self, account: &str) -> Result<usize> {
        Ok(self.0.query_row(
            "SELECT count(*) FROM pending_reads WHERE account=?1",
            [account],
            |r| r.get(0),
        )?)
    }

    pub fn mark_deleted_thread(&self, key: &ThreadKey) -> Result<()> {
        self.0.execute(
            "UPDATE messages SET present=0 WHERE account=?1 AND thread=?2",
            params![key.account, key.thread],
        )?;
        Ok(())
    }

    pub fn put_labels(&mut self, account: &str, labels: &[Label]) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM labels WHERE account=?1", [account])?;
        for label in labels {
            tx.execute(
                "INSERT INTO labels VALUES (?1,?2,?3,?4)",
                params![account, label.id, label.name, label.kind],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn labels(&self) -> Result<Vec<(String, Label)>> {
        let mut stmt = self
            .0
            .prepare("SELECT account,id,name,kind FROM labels ORDER BY lower(name)")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                Label {
                    id: r.get(1)?,
                    name: r.get(2)?,
                    kind: r.get(3)?,
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn put_filters(&self, account: &str, filters: &[Filter]) -> Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO filters VALUES (?1,?2)",
            params![account, serde_json::to_string(filters)?],
        )?;
        Ok(())
    }

    pub fn filters(&self, account: &str) -> Result<Vec<Filter>> {
        let value: Option<String> = self
            .0
            .query_row(
                "SELECT rules FROM filters WHERE account=?1",
                [account],
                |r| r.get(0),
            )
            .optional()?;
        Ok(value
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default())
    }

    pub fn queue_mailbox_changes(
        &mut self,
        keys: &[ThreadKey],
        action: &Action,
    ) -> Result<Vec<ThreadKey>> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let serialized = serde_json::to_string(action)?;
        let inbox = action.inbox_override();
        let sql = format!(
            "INSERT INTO mailbox_changes (account,thread,action,inbox)
            SELECT ?1,?2,?3,?4
            WHERE ?4 IS NULL OR EXISTS (
              SELECT 1 FROM messages m WHERE m.account=?1 AND m.thread=?2 AND m.present=1
              AND NOT EXISTS (SELECT 1 FROM tags WHERE msg=m.pk AND label='DRAFT')
              AND {IN_INBOX} <> ?4
            )"
        );
        let mut queued = Vec::new();
        for key in keys {
            if tx.execute(&sql, params![key.account, key.thread, serialized, inbox])? != 0 {
                queued.push(key.clone());
            }
        }
        tx.commit()?;
        Ok(queued)
    }

    pub fn next_mailbox_change(&self, account: &str) -> Result<Option<MailboxChange>> {
        let row: Option<(i64, String, String)> = self
            .0
            .query_row(
                "SELECT id,thread,action FROM mailbox_changes WHERE account=?1 ORDER BY id LIMIT 1",
                [account],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(id, thread, action)| {
            Ok(MailboxChange {
                id,
                key: ThreadKey {
                    account: account.to_owned(),
                    thread,
                },
                action: serde_json::from_str(&action)?,
            })
        })
        .transpose()
    }

    pub fn finish_mailbox_change(&mut self, change: &MailboxChange, confirmed: bool) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if confirmed {
            match change.action.inbox_override() {
                Some(false) => {
                    tx.execute(
                        "DELETE FROM tags WHERE label='INBOX' AND msg IN
                        (SELECT pk FROM messages WHERE account=?1 AND thread=?2 AND present=1)",
                        params![change.key.account, change.key.thread],
                    )?;
                }
                Some(true) => {
                    tx.execute(
                        "INSERT OR IGNORE INTO tags (msg,label) SELECT m.pk,'INBOX'
                        FROM messages m WHERE account=?1 AND thread=?2 AND present=1
                          AND NOT EXISTS (SELECT 1 FROM tags WHERE msg=m.pk AND label='DRAFT')",
                        params![change.key.account, change.key.thread],
                    )?;
                }
                None => {}
            }
        }
        tx.execute(
            "DELETE FROM mailbox_changes WHERE id=?1 AND account=?2",
            params![change.id, change.key.account],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn list(
        &self,
        view: &View,
        accounts: &[String],
        online: &[ThreadKey],
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Row>> {
        if accounts.is_empty() {
            return Ok(vec![]);
        }
        let mut values: Vec<Value> = accounts.iter().cloned().map(Value::Text).collect();
        let placeholders = std::iter::repeat_n("?", accounts.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut conditions = vec![format!("m.present=1 AND m.account IN ({placeholders})")];
        let hide_spam = match view {
            View::Tag(tag) => !matches!(tag.as_str(), "SPAM" | "TRASH"),
            View::Search(query) => ![
                "in:anywhere",
                "in:all",
                "in:spam",
                "in:trash",
                "label:spam",
                "label:trash",
                "is:spam",
                "is:trash",
            ]
            .iter()
            .any(|op| query.to_ascii_lowercase().contains(op)),
            View::Online | View::Drafts => false,
            _ => true,
        };
        if hide_spam {
            conditions.push(
                "NOT EXISTS (SELECT 1 FROM tags WHERE msg=m.pk AND label IN ('SPAM','TRASH'))"
                    .into(),
            );
        }
        if !matches!(view, View::Drafts | View::Online)
            && !matches!(view, View::Search(query) if ["in:drafts","in:draft","is:draft","label:draft"].iter().any(|op| query.to_ascii_lowercase().contains(op)))
        {
            conditions
                .push("NOT EXISTS (SELECT 1 FROM tags WHERE msg=m.pk AND label='DRAFT')".into());
        }
        let open_snooze = "EXISTS (SELECT 1 FROM snoozes s WHERE s.account=m.account AND s.thread=m.thread AND s.due > ?)";
        match view {
            View::Inbox | View::Snoozed => {
                let filter = if !matches!(view, View::Snoozed) {
                    format!("{IN_INBOX} AND NOT {open_snooze}")
                } else {
                    open_snooze.into()
                };
                conditions.push(filter);
                values.push(Value::Integer(now()));
            }
            View::Tag(tag) if tag == "INBOX" => conditions.push(IN_INBOX.into()),
            View::Tag(tag) => {
                conditions.push("EXISTS (SELECT 1 FROM tags WHERE msg=m.pk AND label=?)".into());
                values.push(Value::Text(tag.clone()));
            }
            View::All => {}
            View::Search(query) => {
                let compiled = search::compile(query)?;
                conditions.push(format!("({})", compiled.clause));
                values.extend(compiled.values);
            }
            View::Online => {
                if online.is_empty() {
                    return Ok(vec![]);
                }
                // The server limits this page. Account plus thread ID keeps different mailboxes isolated.
                let pairs = std::iter::repeat_n("(?,?)", online.len())
                    .collect::<Vec<_>>()
                    .join(",");
                conditions.push(format!("(m.account,m.thread) IN (VALUES {pairs})"));
                for key in online {
                    values.push(Value::Text(key.account.clone()));
                    values.push(Value::Text(key.thread.clone()));
                }
            }
            View::Drafts => conditions
                .push("EXISTS (SELECT 1 FROM tags WHERE msg=m.pk AND label='DRAFT')".into()),
            View::Outbox => return Ok(vec![]),
        }
        values.extend([
            Value::Integer(limit.try_into()?),
            Value::Integer(offset.try_into()?),
        ]);
        let where_clause = conditions.join(" AND ");
        // Choose the small page first so reading message metadata cannot drive the mailbox scan.
        let sql = format!("
            WITH latest AS (
              SELECT m.pk,m.account,m.thread,m.date,
                     row_number() OVER (PARTITION BY m.account,m.thread ORDER BY m.date DESC,m.pk DESC) AS pos
              FROM messages m WHERE {where_clause}
            ), page AS MATERIALIZED (
              SELECT pk,account,thread,date FROM latest WHERE pos=1 ORDER BY date DESC,account,thread LIMIT ? OFFSET ?
            )
            SELECT p.account,p.thread,
                   (SELECT json_group_array(s.sender) FROM (
                     SELECT a.sender FROM messages a
                       WHERE a.account=p.account AND a.thread=p.thread AND a.present=1 AND a.sender<>''
                       GROUP BY a.sender ORDER BY min(a.date),min(a.pk)
                   ) s),
                   m.subject,m.snippet,p.date,
                   (SELECT count(*) FROM messages a
                      WHERE a.account=p.account AND a.thread=p.thread AND a.present=1),
                   EXISTS (SELECT 1 FROM messages a
                      WHERE a.account=p.account AND a.thread=p.thread AND a.present=1 AND a.has_files=1),
                   coalesce((SELECT json_group_array(DISTINCT t.label) FROM messages b JOIN tags t ON t.msg=b.pk
                       WHERE b.account=p.account AND b.thread=p.thread AND b.present=1),'[]')
            FROM page p CROSS JOIN messages m ON m.pk=p.pk
            ORDER BY p.date DESC,p.account,p.thread");
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values), |r| {
            let labels: Vec<String> =
                serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default();
            let has = |id| labels.iter().any(|tag| tag == id);
            Ok(Row {
                key: ThreadKey {
                    account: r.get(0)?,
                    thread: r.get(1)?,
                },
                senders: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
                subject: r.get(3)?,
                snippet: r.get(4)?,
                date: r.get(5)?,
                count: r.get(6)?,
                attachments: r.get(7)?,
                unread: has("UNREAD"),
                starred: has("STARRED"),
                labels,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn thread(&self, key: &ThreadKey) -> Result<Vec<Message>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS}
            FROM messages m WHERE account=?1 AND thread=?2 AND present=1 ORDER BY date,pk"
        ))?;
        let rows = stmt.query_map(params![key.account, key.thread], read_message)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn thread_page(
        &self,
        key: &ThreadKey,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Message>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS}
            FROM messages m WHERE account=?1 AND thread=?2 AND present=1
            ORDER BY date DESC,pk DESC LIMIT ?3 OFFSET ?4"
        ))?;
        let rows = stmt.query_map(
            params![key.account, key.thread, limit, offset],
            read_message,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn cached_message(&self, account: &str, id: &str) -> Result<Option<Message>> {
        Ok(self
            .0
            .query_row(
                &format!(
                    "SELECT {MESSAGE_COLUMNS}
            FROM messages m WHERE account=?1 AND id=?2 AND present=1"
                ),
                params![account, id],
                read_message,
            )
            .optional()?)
    }

    pub fn recent_contact_headers(&self, account: &str) -> Result<Vec<ContactHeaders>> {
        let mut stmt = self.0.prepare("SELECT m.sender,m.recipients,m.cc,
                EXISTS (SELECT 1 FROM tags WHERE msg=m.pk AND label='SENT')
            FROM messages m WHERE m.present=1 AND m.account=?1
                AND NOT EXISTS (SELECT 1 FROM tags WHERE msg=m.pk AND label IN ('SPAM','TRASH','DRAFT'))
            ORDER BY m.date DESC,m.pk DESC LIMIT 1200")?;
        let rows = stmt.query_map([account], |r| {
            Ok(ContactHeaders {
                sender: r.get(0)?,
                recipients: r.get(1)?,
                cc: r.get(2)?,
                sent: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn sent_examples(&self, key: &ThreadKey) -> Result<Vec<MailExcerpt>> {
        let mut stmt = self.0.prepare("SELECT m.sender,m.recipients,m.subject,substr(m.body,1,1800),m.date
            FROM tags s JOIN messages m ON m.pk=s.msg
            WHERE s.label='SENT' AND m.present=1 AND m.account=?1 AND m.thread<>?2
                AND NOT EXISTS (SELECT 1 FROM tags WHERE msg=m.pk AND label IN ('SPAM','TRASH','DRAFT'))
            ORDER BY m.date DESC,m.pk DESC LIMIT 80")?;
        let rows = stmt.query_map(params![key.account, key.thread], read_excerpt)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn search_excerpts(&self, key: &ThreadKey, expression: &str) -> Result<Vec<MailExcerpt>> {
        let mut stmt = self.0.prepare("SELECT m.sender,m.recipients,m.subject,
                    snippet(mail_fts, 1, '', '', ' … ', 35),m.date
            FROM mail_fts JOIN messages m ON m.pk=mail_fts.rowid
            WHERE mail_fts MATCH ?1 AND m.present=1 AND m.account=?2 AND m.thread<>?3
                AND NOT EXISTS (SELECT 1 FROM tags WHERE msg=m.pk AND label IN ('SPAM','TRASH','DRAFT'))
            ORDER BY rank,m.date DESC LIMIT 4")?;
        let rows = stmt.query_map(params![expression, key.account, key.thread], read_excerpt)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn download(&self, account: &str, id: &str, part: &Download) -> Result<Vec<u8>> {
        let raw: Vec<u8> = self.0.query_row(
            "SELECT raw FROM messages WHERE account=?1 AND id=?2",
            params![account, id],
            |r| r.get(0),
        )?;
        match part {
            Download::Raw => Ok(raw),
            Download::Attachment(index) => mime::attachment(&raw, *index),
        }
    }

    pub fn inbox_reader_keys(&self, accounts: &[&str]) -> Result<Vec<(String, String)>> {
        if accounts.is_empty() {
            return Ok(vec![]);
        }
        let inbox = inbox_reader_filter();
        let placeholders = std::iter::repeat_n("?", accounts.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = self.0.prepare_cached(&format!(
            "WITH candidates(pk) AS (
               SELECT msg FROM tags WHERE label='INBOX'
               UNION
               SELECT m.pk FROM mailbox_changes q
               CROSS JOIN messages m ON m.account=q.account AND m.thread=q.thread
               WHERE q.inbox=1 AND m.present=1
             )
             SELECT m.account, m.id FROM candidates c CROSS JOIN messages m ON m.pk=c.pk
             WHERE m.present=1 AND {inbox} AND m.account IN ({placeholders})
             ORDER BY m.date DESC, m.pk DESC"
        ))?;
        let values = std::iter::once(Value::Integer(now())).chain(
            accounts
                .iter()
                .map(|account| Value::Text((*account).to_owned())),
        );
        let keys = stmt.query_map(params_from_iter(values), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        Ok(keys.collect::<rusqlite::Result<_>>()?)
    }

    pub fn reader_source(
        &self,
        account: &str,
        id: &str,
        precache_max_bytes: Option<usize>,
    ) -> Result<Option<(Vec<u8>, String)>> {
        let mut sql = "SELECT m.raw, m.body FROM messages m
                       WHERE m.account=?1 AND m.id=?2 AND m.present=1"
            .to_owned();
        let mut values = vec![Value::Text(account.into()), Value::Text(id.into())];
        if let Some(max_bytes) = precache_max_bytes {
            sql.push_str(&format!(" AND m.size<=?3 AND {}", inbox_reader_filter()));
            values.extend([Value::Integer(max_bytes.try_into()?), Value::Integer(now())]);
        }
        Ok(self
            .0
            .prepare_cached(&sql)?
            .query_row(params_from_iter(values), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?)
    }

    pub fn remote_image(&self, url: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .0
            .query_row("SELECT data FROM remote_images WHERE url=?1", [url], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn save_remote_image(&self, url: &str, bytes: &[u8]) -> Result<()> {
        self.0.execute(
            "INSERT OR IGNORE INTO remote_images VALUES (?1,?2)",
            params![url, bytes],
        )?;
        Ok(())
    }

    pub fn snooze(&self, key: &ThreadKey, due: i64) -> Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO snoozes VALUES (?1,?2,?3)",
            params![key.account, key.thread, due],
        )?;
        Ok(())
    }

    pub fn save_draft(&self, draft: &mut Draft) -> Result<()> {
        let data = serde_json::to_string(draft)?;
        if draft.id == 0 {
            draft.id = self.0.query_row(
                "INSERT INTO drafts(account,data,modified) VALUES (?1,?2,?3) RETURNING id",
                params![draft.account, data, now()],
                |r| r.get(0),
            )?;
        } else {
            self.0.execute(
                "UPDATE drafts SET account=?1,data=?2,modified=?3 WHERE id=?4 AND state='draft'",
                params![draft.account, data, now(), draft.id],
            )?;
        }
        Ok(())
    }

    pub fn drafts(&self) -> Result<Vec<Draft>> {
        let mut stmt = self
            .0
            .prepare("SELECT id,data FROM drafts WHERE state='draft' ORDER BY modified DESC")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|r| {
            let (id, data) = r?;
            let mut draft: Draft = serde_json::from_str(&data)?;
            draft.id = id;
            Ok(draft)
        })
        .collect()
    }

    pub fn discard_draft(&self, id: i64) -> Result<()> {
        self.0.execute(
            "UPDATE drafts SET state='discarded' WHERE id=?1 AND state='draft'",
            [id],
        )?;
        Ok(())
    }

    pub fn stage_file(&self, path: &Path) -> Result<DraftFile> {
        use std::io::Read;
        let file = std::fs::File::open(path)
            .with_context(|| format!("Could not read {}", path.display()))?;
        ensure!(file.metadata()?.is_file(), "Select a regular file");
        let mut bytes = Vec::new();
        file.take(mime::MAX_ATTACHMENTS as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= mime::MAX_ATTACHMENTS,
            "Attachment exceeds 24 MiB"
        );
        let name = mime::filename(
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("attachment"),
        );
        let id = self.0.query_row(
            "INSERT INTO draft_files (data) VALUES (?1) RETURNING id",
            [bytes],
            |r| r.get(0),
        )?;
        Ok(DraftFile {
            name,
            source: DraftFileSource::Staged(id),
        })
    }

    fn draft_file(&self, source: &DraftFileSource) -> Result<Vec<u8>> {
        match source {
            DraftFileSource::Staged(id) => {
                Ok(self
                    .0
                    .query_row("SELECT data FROM draft_files WHERE id=?1", [id], |r| {
                        r.get(0)
                    })?)
            }
            DraftFileSource::Cached {
                account,
                message,
                index,
            } => self.download(account, message, &Download::Attachment(*index)),
        }
    }

    pub fn queue(&mut self, draft: &mut Draft, account: &Account, due: i64) -> Result<i64> {
        let raw = mime::encode(draft, account, |source| self.draft_file(source))?;
        self.save_draft(draft)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            tx.execute(
                "UPDATE drafts SET state='queued' WHERE id=?1 AND state='draft'",
                [draft.id]
            )? == 1,
            "This draft is already queued"
        );
        let id = tx.query_row("INSERT INTO outbox(account,draft,subject,raw,thread,due) VALUES (?1,?2,?3,?4,?5,?6) RETURNING id",
            params![draft.account, draft.id, draft.subject, raw, draft.thread, due], |r| r.get(0))?;
        tx.commit()?;
        Ok(id)
    }

    pub fn outbox(&self) -> Result<Vec<Outgoing>> {
        let mut stmt = self.0.prepare("SELECT id,account,subject,due,status,note FROM outbox WHERE status NOT IN ('sent','canceled') ORDER BY due")?;
        let rows = stmt.query_map([], |r| {
            Ok(Outgoing {
                id: r.get(0)?,
                account: r.get(1)?,
                subject: r.get(2)?,
                due: r.get(3)?,
                status: r.get(4)?,
                note: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn recover_send(&self) -> Result<()> {
        self.0.execute("UPDATE outbox SET status='uncertain',note='The app closed during sending. Check Sent before trying again.' WHERE status='sending'", [])?;
        Ok(())
    }

    pub fn cancel_send(&mut self, id: i64) -> Result<Option<Draft>> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id: Option<i64> = tx.query_row(
            "UPDATE outbox SET status='canceled' WHERE id=?1 AND status IN ('queued','uncertain') RETURNING draft",
            [id], |r| r.get(0)).optional()?;
        let draft = if let Some(id) = id {
            let data: String = tx.query_row(
                "UPDATE drafts SET state='draft',modified=?2 WHERE id=?1 RETURNING data",
                params![id, now()],
                |r| r.get(0),
            )?;
            let mut draft = serde_json::from_str::<Draft>(&data)?;
            draft.id = id;
            Some(draft)
        } else {
            None
        };
        tx.commit()?;
        Ok(draft)
    }

    pub fn send_waiting(&self, account: &str) -> Result<bool> {
        Ok(self.0.query_row(
            "SELECT EXISTS (SELECT 1 FROM outbox WHERE account=?1 AND status='queued' AND due<=?2)",
            params![account, now()],
            |r| r.get(0),
        )?)
    }

    pub fn next_send(&self, account: &str) -> Result<Option<QueuedMail>> {
        Ok(self.0.query_row("
            UPDATE outbox SET status='sending' WHERE id=(
                SELECT id FROM outbox WHERE account=?1 AND status='queued' AND due<=?2 ORDER BY due,id LIMIT 1
            ) RETURNING id,raw,thread", params![account, now()],
            |r| Ok(QueuedMail { id: r.get(0)?, raw: r.get(1)?, thread: r.get(2)? })).optional()?)
    }

    pub fn complete_send(&mut self, id: i64, error: Option<&str>) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(error) = error {
            tx.execute(
                "UPDATE outbox SET status='uncertain',note=?2 WHERE id=?1",
                params![id, error],
            )?;
        } else {
            tx.execute("UPDATE outbox SET status='sent' WHERE id=?1", [id])?;
            tx.execute(
                "UPDATE drafts SET state='sent' WHERE id=(SELECT draft FROM outbox WHERE id=?1)",
                [id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn save_search(&self, name: &str, query: &str, account: Option<&str>) -> Result<()> {
        search::compile(query)?;
        self.0.execute(
            "INSERT INTO saved_searches (name,query,account) VALUES (?1,?2,?3)",
            params![name, query, account],
        )?;
        Ok(())
    }

    pub fn saved_searches(&self) -> Result<Vec<SavedSearch>> {
        let mut stmt = self.0.prepare(
            "SELECT id,name,query,account FROM saved_searches WHERE active=1 ORDER BY lower(name)",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SavedSearch {
                id: r.get(0)?,
                name: r.get(1)?,
                query: r.get(2)?,
                account: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn hide_search(&self, id: i64) -> Result<()> {
        self.0
            .execute("UPDATE saved_searches SET active=0 WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn dark(&self) -> Result<bool> {
        Ok(self
            .0
            .query_row(
                "SELECT value='dark' FROM settings WHERE id='appearance'",
                [],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(true))
    }

    pub fn set_dark(&self, dark: bool) -> Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO settings VALUES ('appearance',?1)",
            [if dark { "dark" } else { "light" }],
        )?;
        Ok(())
    }

    pub fn body_view(&self, account: &str, message: &str) -> Result<BodyView> {
        let key = format!("body-view/{account}/{message}");
        let value: Option<String> = self
            .0
            .query_row("SELECT value FROM settings WHERE id=?1", [key], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(match value.as_deref() {
            Some("original") => BodyView::Original,
            Some("selectable") => BodyView::Selectable,
            _ => BodyView::Auto,
        })
    }

    pub fn set_body_view(&self, account: &str, message: &str, view: BodyView) -> Result<()> {
        let key = format!("body-view/{account}/{message}");
        let value = match view {
            BodyView::Auto => {
                self.0.execute("DELETE FROM settings WHERE id=?1", [key])?;
                return Ok(());
            }
            BodyView::Original => "original",
            BodyView::Selectable => "selectable",
        };
        self.0.execute(
            "INSERT OR REPLACE INTO settings VALUES (?1,?2)",
            params![key, value],
        )?;
        Ok(())
    }
}

fn inbox_reader_filter() -> String {
    format!(
        "({IN_INBOX})
        AND NOT EXISTS (SELECT 1 FROM tags WHERE msg=m.pk AND label IN ('SPAM','TRASH','DRAFT'))
        AND NOT EXISTS (SELECT 1 FROM snoozes s
            WHERE s.account=m.account AND s.thread=m.thread AND s.due>?)"
    )
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn read_excerpt(row: &rusqlite::Row<'_>) -> rusqlite::Result<MailExcerpt> {
    Ok(MailExcerpt {
        sender: row.get(0)?,
        recipients: row.get(1)?,
        subject: row.get(2)?,
        body: row.get(3)?,
        date: row.get(4)?,
    })
}

fn read_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        sender: r.get(1)?,
        recipients: r.get(2)?,
        cc: r.get(3)?,
        bcc: r.get(4)?,
        reply_to: r.get(5)?,
        subject: r.get(6)?,
        body: r.get(7)?,
        date: r.get(8)?,
        message_id: r.get(9)?,
        references: r.get(10)?,
        files: serde_json::from_str(&r.get::<_, String>(11)?).unwrap_or_default(),
        labels: serde_json::from_str(&r.get::<_, String>(12)?).unwrap_or_default(),
    })
}
