use std::path::Path;

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use lettre::message::{Attachment, Mailbox, Mailboxes, MultiPart, SinglePart, header::ContentType};
use mail_parser::{Address, MessageParser, MimeHeaders, PartType};

use crate::model::{Account, Draft, DraftFileSource, Message};

pub fn decode(raw: &str) -> Result<Vec<u8>> {
    Ok(URL_SAFE_NO_PAD.decode(raw.trim_end_matches('='))?)
}

fn addresses(value: Option<&Address<'_>>) -> String {
    value
        .into_iter()
        .flat_map(Address::iter)
        .filter_map(|party| {
            let address = party.address()?;
            let name = party.name().filter(|name| !name.is_empty());
            Some(match name {
                Some(name) => format!(
                    "\"{}\" <{address}>",
                    name.replace('\\', "\\\\").replace('"', "\\\"")
                ),
                None => address.to_owned(),
            })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn inspect(raw: &[u8]) -> Message {
    let mut output = Message {
        id: String::new(),
        sender: String::new(),
        recipients: String::new(),
        cc: String::new(),
        bcc: String::new(),
        reply_to: String::new(),
        subject: String::new(),
        body: String::new(),
        date: 0,
        message_id: String::new(),
        references: String::new(),
        labels: vec![],
        files: vec![],
    };
    let Some(mail) = MessageParser::default().parse(raw) else {
        return output;
    };
    output.subject = mail.subject().unwrap_or_default().to_owned();
    output.sender = addresses(mail.from());
    output.recipients = addresses(mail.to());
    output.cc = addresses(mail.cc());
    output.bcc = addresses(mail.bcc());
    output.reply_to = addresses(mail.reply_to());
    output.message_id = mail.message_id().map(bracket).unwrap_or_default();
    output.references = if let Some(ids) = mail.references().as_text_list() {
        ids.iter()
            .map(|id| bracket(id))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        mail.references().as_text().map(bracket).unwrap_or_default()
    };
    output.body = mail
        .text_bodies()
        .filter_map(|part| match &part.body {
            PartType::Text(text) => Some(text.to_string()),
            PartType::Html(html) => {
                Some(html2text::from_read(html.as_bytes(), 105).unwrap_or_default())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    output.files = mail
        .attachments()
        .enumerate()
        .map(|(i, part)| {
            let name = part
                .attachment_name()
                .map(ToString::to_string)
                .unwrap_or_else(|| {
                    if matches!(part.body, PartType::Message(_)) {
                        format!("message-{}.eml", i + 1)
                    } else {
                        format!("attachment-{}", i + 1)
                    }
                });
            (i, name)
        })
        .collect();
    output
}

fn bracket(id: &str) -> String {
    format!("<{}>", id.trim().trim_matches(['<', '>']))
}

pub fn attachment(raw: &[u8], index: usize) -> Result<Vec<u8>> {
    let msg = MessageParser::default()
        .parse(raw)
        .context("Could not decode this message")?;
    Ok(attachment_bytes(&msg, index)?.to_vec())
}

fn attachment_bytes<'a>(msg: &'a mail_parser::Message<'_>, index: usize) -> Result<&'a [u8]> {
    let part = msg
        .attachment(index.try_into()?)
        .context("Attachment unavailable")?;
    let bytes = match &part.body {
        PartType::Message(message) => message.raw_message(),
        PartType::Multipart(_) => msg
            .raw_message()
            .get(part.offset_body as usize..part.offset_end as usize)
            .context("Invalid MIME part")?,
        _ => part.contents(),
    };
    Ok(bytes)
}

pub fn filename(name: &str) -> String {
    let basename = name.rsplit(['/', '\\']).next().unwrap_or("attachment");
    let safe: String = basename
        .chars()
        .filter(|c| !c.is_control())
        .take(160)
        .collect();
    if safe.is_empty() || matches!(safe.as_str(), "." | "..") {
        "attachment".to_owned()
    } else {
        safe
    }
}

pub const MAX_ATTACHMENTS: usize = 24 * 1024 * 1024;

pub fn encode(
    draft: &Draft,
    account: &Account,
    load: impl Fn(&DraftFileSource) -> Result<Vec<u8>>,
) -> Result<String> {
    encode_with_id(draft, account, None, load)
}

pub fn encode_with_id(
    draft: &Draft,
    account: &Account,
    message_id: Option<String>,
    load: impl Fn(&DraftFileSource) -> Result<Vec<u8>>,
) -> Result<String> {
    let mut builder = lettre::Message::builder()
        .from(account.email.parse()?)
        .subject(&draft.subject)
        .message_id(message_id)
        .keep_bcc();
    let mut count = 0;
    for (value, kind) in [(&draft.to, 0), (&draft.cc, 1), (&draft.bcc, 2)] {
        ensure!(
            !value.contains(['\r', '\n']),
            "Recipients must be on one line"
        );
        if value.trim().is_empty() {
            continue;
        }
        for recipient in value
            .parse::<Mailboxes>()
            .context("Check the recipient addresses")?
            .iter()
        {
            count += 1;
            builder = match kind {
                0 => builder.to(recipient.clone()),
                1 => builder.cc(recipient.clone()),
                _ => builder.bcc(recipient.clone()),
            };
        }
    }
    ensure!(count > 0, "Enter at least one recipient");
    ensure!(
        !draft.subject.contains(['\r', '\n']),
        "The subject must be on one line"
    );
    if let Some(value) = &draft.in_reply_to {
        builder = builder.in_reply_to(value.clone());
    }
    if let Some(value) = &draft.references {
        builder = builder.references(value.clone());
    }
    let text = SinglePart::plain(draft.body.clone());
    let alternative = (!draft.formatting.is_empty()).then(|| {
        MultiPart::alternative()
            .singlepart(SinglePart::plain(draft.body.clone()))
            .singlepart(SinglePart::html(crate::compose_format::html(
                &draft.body,
                &draft.formatting,
            )))
    });
    let mail = if draft.files.is_empty() {
        match alternative {
            Some(parts) => builder.multipart(parts)?,
            None => builder.singlepart(text)?,
        }
    } else {
        let mut mixed = match alternative {
            Some(parts) => MultiPart::mixed().multipart(parts),
            None => MultiPart::mixed().singlepart(text),
        };
        let mut available = MAX_ATTACHMENTS;
        for file in &draft.files {
            let bytes = load(&file.source)?;
            ensure!(bytes.len() <= available, "Attachments exceed 24 MiB");
            available -= bytes.len();
            mixed = mixed.singlepart(
                Attachment::new(filename(&file.name))
                    .body(bytes, ContentType::parse("application/octet-stream")?),
            );
        }
        builder.multipart(mixed)?
    };
    let wire = mail.formatted();
    ensure!(
        wire.len() <= 34 * 1024 * 1024,
        "Message exceeds Gmail's upload limit"
    );
    Ok(URL_SAFE_NO_PAD.encode(wire))
}

pub fn reply(message: &Message, account: &Account, thread: &str, all: bool) -> Draft {
    let from = parse_mailboxes(&message.sender);
    let sent_by_me = from
        .iter()
        .any(|mb| mb.email.to_string().eq_ignore_ascii_case(&account.email));
    let mut to = if sent_by_me {
        parse_mailboxes(&message.recipients)
    } else if message.reply_to.is_empty() {
        from
    } else {
        parse_mailboxes(&message.reply_to)
    };
    to.retain(|mb| !mb.email.to_string().eq_ignore_ascii_case(&account.email));
    let mut seen = std::collections::HashSet::from([account.email.to_ascii_lowercase()]);
    to.retain(|mb| seen.insert(mb.email.to_string().to_ascii_lowercase()));
    let cc = if all {
        parse_mailboxes(&message.recipients)
            .into_iter()
            .chain(parse_mailboxes(&message.cc))
            .filter(|mb| seen.insert(mb.email.to_string().to_ascii_lowercase()))
            .collect::<Vec<_>>()
    } else {
        vec![]
    };
    let subject = if message
        .subject
        .get(..3)
        .is_some_and(|s| s.eq_ignore_ascii_case("re:"))
    {
        message.subject.clone()
    } else {
        format!("Re: {}", message.subject)
    };
    let in_reply_to = (!message.message_id.is_empty()).then(|| message.message_id.clone());
    let references = in_reply_to.as_ref().map(|id| {
        [message.references.as_str(), id]
            .join(" ")
            .trim()
            .to_owned()
    });
    Draft {
        account: account.id.clone(),
        to: format_mailboxes(to),
        cc: format_mailboxes(cc),
        subject,
        thread: in_reply_to.as_ref().map(|_| thread.to_owned()),
        in_reply_to,
        references,
        ..Draft::default()
    }
}

pub fn conversation_senders(from: &[String], identity: &str) -> (String, String) {
    let mut seen = std::collections::HashSet::new();
    let mut names = Vec::new();
    let mut details = Vec::new();
    for value in from {
        let mailboxes = parse_mailboxes(value);
        if mailboxes.is_empty() && !value.trim().is_empty() && seen.insert(value.to_lowercase()) {
            names.push((value.clone(), value.clone()));
            details.push(value.clone());
        }
        for mailbox in mailboxes {
            let email = mailbox.email.to_string();
            let is_me = email.eq_ignore_ascii_case(identity);
            let display = mailbox.name.as_deref().filter(|s| !s.trim().is_empty());
            let key = if email.eq_ignore_ascii_case("notification@edstem.org") {
                format!(
                    "{}:{}",
                    email.to_ascii_lowercase(),
                    display.unwrap_or("").to_lowercase()
                )
            } else {
                email.to_ascii_lowercase()
            };
            if !seen.insert(key) {
                continue;
            }
            let full = if is_me {
                "me"
            } else {
                display.unwrap_or(&email)
            };
            let short = if is_me {
                "me"
            } else if let Some(display) = display {
                display.split_whitespace().next().unwrap_or(display)
            } else {
                email.split('@').next().unwrap_or(&email)
            };
            names.push((full.to_owned(), short.to_owned()));
            details.push(mailbox.to_string());
        }
    }
    let caption = match names.as_slice() {
        [] => "Unknown sender".to_owned(),
        [(full, _)] => full.clone(),
        names => names
            .iter()
            .map(|(full, short)| {
                if names
                    .iter()
                    .filter(|(_, other)| other.eq_ignore_ascii_case(short))
                    .count()
                    > 1
                {
                    full.as_str()
                } else {
                    short.as_str()
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
    };
    (
        caption,
        details.join(
            "
",
        ),
    )
}

pub(crate) fn parse_mailboxes(value: &str) -> Vec<Mailbox> {
    value
        .parse::<Mailboxes>()
        .map(|boxes| boxes.iter().cloned().collect())
        .unwrap_or_default()
}

fn format_mailboxes(boxes: Vec<Mailbox>) -> String {
    boxes
        .into_iter()
        .map(|mb| mb.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn save_download(directory: &Path, name: &str, bytes: &[u8]) -> Result<std::path::PathBuf> {
    use std::io::{ErrorKind, Write};

    let name = filename(name);
    let file_name = Path::new(&name);
    let stem = file_name
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(&name);
    let extension = file_name.extension().and_then(|value| value.to_str());
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .with_context(|| format!("Could not save to {}", directory.display()))?;
    file.write_all(bytes)?;

    for number in 0..10_000 {
        let candidate = match (number, extension) {
            (0, _) => directory.join(&name),
            (number, Some(extension)) => directory.join(format!("{stem} ({number}).{extension}")),
            (number, None) => directory.join(format!("{stem} ({number})")),
        };
        match file.persist_noclobber(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(error) if error.error.kind() == ErrorKind::AlreadyExists => file = error.file,
            Err(error) => {
                return Err(error.error)
                    .with_context(|| format!("Could not save {}", candidate.display()));
            }
        }
    }
    anyhow::bail!("Too many files named {name} in {}", directory.display())
}

pub fn save_attachments(
    raw: &[u8],
    directory: &Path,
    files: &[(usize, String)],
) -> Result<Vec<Result<std::path::PathBuf>>> {
    let message = MessageParser::default()
        .parse(raw)
        .context("Could not decode this message")?;
    Ok(files
        .iter()
        .map(|(index, name)| {
            attachment_bytes(&message, *index)
                .and_then(|bytes| save_download(directory, name, bytes))
                .with_context(|| format!("Could not save {}", filename(name)))
        })
        .collect())
}
