use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rmcp::schemars::{self, JsonSchema};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    gmail::Gmail,
    mime,
    model::{Account, Draft},
};

#[derive(Clone, Deserialize, JsonSchema)]
pub struct SendRequest {
    /// Sender account ID from list_accounts.
    pub account: String,
    /// Unique ID for this send, such as a UUID. Reuse it for an identical retry.
    pub request_id: String,
    /// True only when the user explicitly asked to send this exact email.
    pub confirm_send: bool,
    /// Comma-separated email addresses or named addresses.
    pub to: String,
    /// Optional comma-separated CC recipients.
    #[serde(default)]
    pub cc: String,
    /// Optional comma-separated BCC recipients.
    #[serde(default)]
    pub bcc: String,
    pub subject: String,
    /// Plain-text message body.
    pub body: String,
}

#[derive(Serialize, JsonSchema)]
pub struct SentMail {
    pub account: String,
    pub request_id: String,
    /// Gmail's ID for the sent message.
    pub message_id: String,
    /// Message-ID header usable with Gmail's rfc822msgid: search.
    pub rfc_message_id: String,
    /// True when returning the receipt from an earlier successful call.
    pub replayed: bool,
}

pub async fn send(path: PathBuf, account: Account, request: SendRequest) -> Result<SentMail> {
    ensure!(request.confirm_send, "Sending requires confirm_send=true");
    ensure!(
        (1..=128).contains(&request.request_id.len())
            && request
                .request_id
                .bytes()
                .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') }),
        "Use a request ID of 1–128 letters, digits, hyphens, or underscores"
    );
    ensure!(
        !request.to.trim().is_empty(),
        "Enter at least one To recipient"
    );
    ensure!(
        [&request.to, &request.cc, &request.bcc, &request.subject]
            .iter()
            .all(|value| value.len() <= 8192)
            && request.body.len() <= 1024 * 1024,
        "Recipients and subject must each be at most 8192 bytes and the body at most 1 MiB"
    );

    let receipt = Receipt::new(path, &account, &request)?;
    let draft = Draft {
        to: request.to,
        cc: request.cc,
        bcc: request.bcc,
        subject: request.subject,
        body: request.body,
        ..Draft::default()
    };
    let raw = mime::encode_with_id(
        &draft,
        &account,
        Some(receipt.rfc_message_id.clone()),
        |_| anyhow::bail!("No attachment was supplied"),
    )?;

    if let Some(sent) = receipt.access(|receipt, db| receipt.lookup(db)).await? {
        return Ok(sent);
    }
    let gmail = Gmail::new(account)?;
    gmail
        .ready_to_send()
        .await
        .context("Could not authorize sending with Gmail")?;

    // Commit the reservation before sending. A cancellation or lost Gmail response
    // leaves the request reserved so retrying it cannot send another copy.
    if let Some(sent) = receipt.access(|receipt, db| receipt.reserve(db)).await? {
        return Ok(sent);
    }
    let sent = gmail.send(&raw, None).await.with_context(|| {
        format!(
            "Gmail did not confirm the send. Check Sent for {} before trying again",
            receipt.rfc_message_id
        )
    })?;
    let id = sent.id.clone();
    receipt
        .access(move |receipt, db| receipt.complete(db, id))
        .await
        .with_context(|| {
            format!(
                "Gmail accepted the message as {}, but saving the local receipt failed",
                sent.id
            )
        })
}

#[derive(Clone)]
struct Receipt {
    path: PathBuf,
    account: String,
    request_id: String,
    fingerprint: [u8; 32],
    rfc_message_id: String,
}

impl Receipt {
    fn new(path: PathBuf, account: &Account, request: &SendRequest) -> Result<Self> {
        let identity = account.email.to_ascii_lowercase();
        let id = Sha256::digest(serde_json::to_vec(&(
            &account.id,
            &identity,
            &request.request_id,
        ))?);
        let fingerprint = Sha256::digest(serde_json::to_vec(&(
            identity,
            &request.to,
            &request.cc,
            &request.bcc,
            &request.subject,
            &request.body,
        ))?)
        .into();
        Ok(Self {
            path,
            account: account.id.clone(),
            request_id: request.request_id.clone(),
            fingerprint,
            rfc_message_id: format!("<tandem-{}@tandem-mail.local>", URL_SAFE_NO_PAD.encode(id)),
        })
    }

    async fn access<T: Send + 'static>(
        &self,
        task: impl FnOnce(&Self, &Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let receipt = self.clone();
        tokio::task::spawn_blocking(move || {
            let db = Connection::open_with_flags(&receipt.path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
            db.busy_timeout(Duration::from_secs(8))?;
            db.execute_batch(
                "PRAGMA synchronous=FULL;
                 CREATE TABLE IF NOT EXISTS mcp_send_receipts (
                     account TEXT NOT NULL,
                     request_id TEXT NOT NULL,
                     fingerprint BLOB NOT NULL,
                     gmail_message_id TEXT,
                     PRIMARY KEY (account, request_id)
                 );",
            )?;
            task(&receipt, &db)
        })
        .await
        .context("Send receipt worker stopped")?
    }

    fn lookup(&self, db: &Connection) -> Result<Option<SentMail>> {
        let existing = db
            .query_row(
                "SELECT fingerprint, gmail_message_id FROM mcp_send_receipts
             WHERE account=?1 AND request_id=?2",
                params![self.account, self.request_id],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let Some((fingerprint, message_id)) = existing else {
            return Ok(None);
        };
        ensure!(
            fingerprint == self.fingerprint,
            "This request ID already belongs to a different email"
        );
        let message_id = message_id.with_context(|| format!(
            "This send is in progress or its outcome is unknown. Check Sent for {} before trying again", self.rfc_message_id
        ))?;
        Ok(Some(self.sent(message_id, true)))
    }

    fn reserve(&self, db: &Connection) -> Result<Option<SentMail>> {
        let inserted = db.execute(
            "INSERT INTO mcp_send_receipts (account, request_id, fingerprint)
             VALUES (?1, ?2, ?3) ON CONFLICT(account, request_id) DO NOTHING",
            params![self.account, self.request_id, self.fingerprint.as_slice()],
        )?;
        if inserted == 0 {
            let existing = self.lookup(db)?.context("Send reservation disappeared")?;
            return Ok(Some(existing));
        }
        Ok(None)
    }

    fn complete(&self, db: &Connection, message_id: String) -> Result<SentMail> {
        let updated = db.execute(
            "UPDATE mcp_send_receipts SET gmail_message_id=?3
             WHERE account=?1 AND request_id=?2 AND gmail_message_id IS NULL",
            params![self.account, self.request_id, message_id],
        )?;
        ensure!(
            updated == 1,
            "Send reservation is missing or already complete"
        );
        Ok(self.sent(message_id, false))
    }

    fn sent(&self, message_id: String, replayed: bool) -> SentMail {
        SentMail {
            account: self.account.clone(),
            request_id: self.request_id.clone(),
            message_id,
            rfc_message_id: self.rfc_message_id.clone(),
            replayed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn concurrent_sends_reserve_once_and_retry_replays_the_receipt() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("mail.sqlite");
        Connection::open(&path)?.execute_batch("PRAGMA journal_mode=WAL;")?;
        let account = Account {
            id: "home".into(),
            name: "Personal".into(),
            email: "sender@example.com".into(),
            credentials: directory.path().join("no-credentials.json"),
        };
        let mut request = SendRequest {
            account: "home".into(),
            request_id: "retry-example".into(),
            confirm_send: true,
            to: "recipient@example.com".into(),
            cc: String::new(),
            bcc: "private@example.com".into(),
            subject: "Test message".into(),
            body: "Hello.".into(),
        };
        let receipt = Receipt::new(path.clone(), &account, &request)?;
        let (first, second) = tokio::join!(
            receipt.access(|receipt, db| receipt.reserve(db)),
            receipt.access(|receipt, db| receipt.reserve(db))
        );
        let pending = match (first, second) {
            (Ok(None), Err(error)) | (Err(error), Ok(None)) => error,
            _ => anyhow::bail!("Exactly one concurrent request should reserve the send"),
        };
        assert!(pending.to_string().contains(&receipt.rfc_message_id));

        let sent = receipt
            .access(|receipt, db| receipt.complete(db, "gmail-test-id".into()))
            .await?;
        assert!(!sent.replayed);
        let replayed = send(path.clone(), account.clone(), request.clone()).await?;
        assert!(replayed.replayed);
        assert_eq!(replayed.message_id, sent.message_id);
        assert_eq!(replayed.rfc_message_id, sent.rfc_message_id);

        request.to = "different@example.com".into();
        let error = send(path, account, request)
            .await
            .err()
            .context("The request ID was reused for a different recipient")?;
        assert!(error.to_string().contains("belongs to a different email"));
        Ok(())
    }
}
