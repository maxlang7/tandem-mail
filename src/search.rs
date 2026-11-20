use anyhow::{Context, Result, bail, ensure};
use chrono::{Duration, Local, NaiveDate, TimeZone};
use rusqlite::types::Value;

use crate::store::IN_INBOX;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Token {
    Value(String),
    Open(bool),
    Close(bool),
    Or,
    And,
    Not,
}

fn lex(input: &str) -> Result<Vec<Token>> {
    let mut chars = input.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '(' => tokens.push(Token::Open(false)),
            '{' => tokens.push(Token::Open(true)),
            ')' => tokens.push(Token::Close(false)),
            '}' => tokens.push(Token::Close(true)),
            '-' => tokens.push(Token::Not),
            first => {
                let mut value = String::new();
                let mut quoted = false;
                let mut used_quotes = false;
                let mut current = Some(first);
                while let Some(c) = current {
                    if c == '"' {
                        quoted = !quoted;
                        used_quotes = true;
                    } else if c == '\\'
                        && quoted
                        && chars.peek().is_some_and(|c| matches!(c, '"' | '\\'))
                    {
                        value.extend(chars.next());
                    } else {
                        value.push(c);
                    }
                    if !quoted
                        && chars.peek().is_some_and(|c| {
                            c.is_whitespace() || matches!(c, '(' | ')' | '{' | '}')
                        })
                    {
                        break;
                    }
                    current = chars.next();
                }
                ensure!(!quoted, "Close the quoted phrase");
                tokens.push(if !used_quotes && value == "OR" {
                    Token::Or
                } else if !used_quotes && value == "AND" {
                    Token::And
                } else {
                    Token::Value(value)
                });
            }
        }
        ensure!(tokens.len() <= 120, "Search is too long");
    }
    Ok(tokens)
}

#[derive(Default)]
pub struct Sql {
    pub clause: String,
    pub values: Vec<Value>,
}

pub fn compile(input: &str) -> Result<Sql> {
    let tokens = lex(input)?;
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
        values: vec![],
    };
    let clause = if tokens.is_empty() {
        "1".to_owned()
    } else {
        parser.expr(false)?
    };
    ensure!(parser.pos == tokens.len(), "Unexpected closing bracket");
    Ok(Sql {
        clause,
        values: parser.values,
    })
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    values: Vec<Value>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn expr(&mut self, braces: bool) -> Result<String> {
        let mut terms = vec![self.and_group(braces)?];
        while self.peek() == Some(&Token::Or) || (braces && self.has_term()) {
            if self.peek() == Some(&Token::Or) {
                self.pos += 1;
            }
            terms.push(self.and_group(braces)?);
        }
        Ok(terms.join(" OR "))
    }

    fn has_term(&self) -> bool {
        matches!(
            self.peek(),
            Some(Token::Value(_) | Token::Open(_) | Token::Not)
        )
    }

    fn and_group(&mut self, braces: bool) -> Result<String> {
        let mut terms = vec![self.term()?];
        while self.peek() == Some(&Token::And) || (!braces && self.has_term()) {
            if self.peek() == Some(&Token::And) {
                self.pos += 1;
            }
            terms.push(self.term()?);
        }
        Ok(terms.join(" AND "))
    }

    fn term(&mut self) -> Result<String> {
        let token = self
            .tokens
            .get(self.pos)
            .context("Expected a search term")?;
        self.pos += 1;
        match token {
            Token::Value(raw) => self.predicate(raw),
            Token::Not => Ok(format!("NOT ({})", self.term()?)),
            Token::Open(braces) => {
                let inner = self.expr(*braces)?;
                ensure!(
                    self.peek() == Some(&Token::Close(*braces)),
                    "Close the search group"
                );
                self.pos += 1;
                Ok(format!("({inner})"))
            }
            _ => bail!("Expected a search term"),
        }
    }

    fn bind(&mut self, sql: &str, value: impl Into<Value>) -> String {
        self.values.push(value.into());
        sql.to_owned()
    }

    fn predicate(&mut self, raw: &str) -> Result<String> {
        let mut field = "";
        let mut value = raw;
        if let Some((possible, rest)) = raw.split_once(':')
            && !possible.is_empty()
            && possible
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            field = possible;
            value = rest;
        }
        ensure!(!value.is_empty(), "Give {field} a value");
        let lower = value.to_lowercase();
        let sql = match field {
            "" | "subject" | "filename" => {
                let column = match field {
                    "subject" => "subject : ",
                    "filename" => "files : ",
                    _ => "",
                };
                self.bind(
                    "m.pk IN (SELECT rowid FROM mail_fts WHERE mail_fts MATCH ?)",
                    format!("{column}\"{}\"", value.replace('"', "\"\"")),
                )
            }
            "from" | "to" | "cc" | "bcc" | "rfc822msgid" => {
                let column = match field {
                    "from" => "sender",
                    "to" => "recipients",
                    "cc" => "cc",
                    "bcc" => "bcc",
                    _ => "rfc_id",
                };
                self.bind(&format!("instr(lower(m.{column}), ?) > 0"), lower)
            }
            "in" if lower == "anywhere" || lower == "all" => "1".into(),
            "in" if lower == "archive" || lower == "archived" => {
                format!(
                    "NOT {IN_INBOX} AND NOT EXISTS (SELECT 1 FROM tags WHERE msg=m.pk AND label IN ('SPAM','TRASH','DRAFT'))"
                )
            }
            "is" | "in" | "label" if lower == "inbox" => IN_INBOX.into(),
            "is" if lower == "read" => {
                "NOT EXISTS (SELECT 1 FROM tags WHERE msg=m.pk AND label='UNREAD')".into()
            }
            "is" | "in" | "label" | "category" => {
                let tag = match lower.as_str() {
                    "sent" | "read" | "unread" | "starred" | "important" | "inbox" | "spam"
                    | "trash" => lower.to_ascii_uppercase(),
                    "drafts" | "draft" => "DRAFT".into(),
                    "primary" if field == "category" => "CATEGORY_PERSONAL".into(),
                    _ if field == "category" => format!("CATEGORY_{}", lower.to_ascii_uppercase()),
                    _ => value.to_owned(),
                };
                if field == "is"
                    && !matches!(
                        tag.as_str(),
                        "UNREAD"
                            | "STARRED"
                            | "IMPORTANT"
                            | "SENT"
                            | "DRAFT"
                            | "TRASH"
                            | "SPAM"
                            | "INBOX"
                    )
                {
                    bail!("Use Gmail search for is:{value}");
                }
                self.bind(
                    "EXISTS (SELECT 1 FROM tags t LEFT JOIN labels l ON l.account=m.account AND t.label=l.id
                     WHERE t.msg=m.pk AND (lower(?) IN (lower(t.label), lower(l.name))))", tag)
            }
            "has" if lower == "attachment" => "m.has_files=1".into(),
            "has" if lower == "userlabels" || lower == "nouserlabels" => {
                let prefix = if lower == "nouserlabels" { "NOT " } else { "" };
                format!(
                    "{prefix}EXISTS (SELECT 1 FROM tags t JOIN labels l ON l.account=m.account AND t.label=l.id WHERE t.msg=m.pk AND l.kind='user')"
                )
            }
            "after" | "newer" | "before" | "older" => {
                let timestamp = date(value)?;
                let cmp = if matches!(field, "after" | "newer") {
                    ">="
                } else {
                    "<"
                };
                self.bind(&format!("m.date {cmp} ?"), timestamp)
            }
            "newer_than" | "older_than" => {
                let (count, suffix) = number_and_suffix(&lower)?;
                let multiplier = match suffix {
                    "d" => 1,
                    "m" => 30,
                    "y" => 365,
                    _ => bail!("Use d, m, or y for relative dates"),
                };
                let days = count
                    .checked_mul(multiplier)
                    .and_then(Duration::try_days)
                    .context("Relative date is too large")?;
                let timestamp = Local::now()
                    .checked_sub_signed(days)
                    .context("Relative date is too large")?
                    .timestamp_millis();
                let cmp = if field == "newer_than" { ">=" } else { "<" };
                self.bind(&format!("m.date {cmp} ?"), timestamp)
            }
            "larger" | "size" | "smaller" => {
                let (count, suffix) = number_and_suffix(&lower)?;
                let multiplier = match suffix {
                    "" => 1,
                    "k" | "kb" => 1024,
                    "m" | "mb" => 1024 * 1024,
                    _ => bail!("Use bytes, K, or M for size"),
                };
                let size = count
                    .checked_mul(multiplier)
                    .context("Message size is too large")?;
                let cmp = if field == "smaller" { "<" } else { ">" };
                self.bind(&format!("m.size {cmp} ?"), size)
            }
            "account" => self.bind("m.account=?", lower),
            _ => bail!("Use Gmail search for {field}:{value}"),
        };
        Ok(sql)
    }
}

fn date(value: &str) -> Result<i64> {
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return value
            .parse::<i64>()?
            .checked_mul(1000)
            .context("Date is too large");
    }
    let day = NaiveDate::parse_from_str(&value.replace('/', "-"), "%Y-%m-%d")
        .context("Use YYYY-MM-DD for dates")?;
    Ok(Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .context("Date unavailable in your time zone")?
        .timestamp_millis())
}

fn number_and_suffix(value: &str) -> Result<(i64, &str)> {
    let at = value.bytes().take_while(u8::is_ascii_digit).count();
    Ok((
        value[..at].parse().context("Enter a positive number")?,
        &value[at..],
    ))
}

pub fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
