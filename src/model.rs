use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub email: String,
    pub credentials: PathBuf,
}

pub fn accounts(home: &Path, app_dir: &Path) -> anyhow::Result<Vec<Account>> {
    let custom = app_dir.join("accounts.json");
    let accounts: Vec<Account> = if custom.exists() {
        serde_json::from_slice(&std::fs::read(custom)?)?
    } else {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        [
            ("home", "Personal", "max@langhorst.com"),
            ("cmu", "CMU", "mlanghor@andrew.cmu.edu"),
        ]
        .into_iter()
        .map(|(id, name, email)| Account {
            id: id.into(),
            name: name.into(),
            email: email.into(),
            credentials: config.join(format!("google-workspace-{id}/credentials/{email}.json")),
        })
        .collect()
    };
    let mut ids = std::collections::HashSet::new();
    for account in &accounts {
        anyhow::ensure!(
            !account.id.is_empty() && ids.insert(&account.id),
            "Account IDs must be unique and nonempty"
        );
        anyhow::ensure!(
            account.email.contains('@'),
            "Enter an email address for {}",
            account.id
        );
    }
    Ok(accounts)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteMessage {
    pub id: String,
    pub thread_id: String,
    #[serde(default)]
    pub label_ids: Vec<String>,
    #[serde(default)]
    pub internal_date: String,
    pub raw: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Label {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ThreadKey {
    pub account: String,
    pub thread: String,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub key: ThreadKey,
    pub reader_message: String,
    pub senders: Vec<String>,
    pub subject: String,
    pub snippet: String,
    pub date: i64,
    pub unread: bool,
    pub starred: bool,
    pub attachments: bool,
    pub count: usize,
    pub labels: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub id: String,
    pub sender: String,
    pub recipients: String,
    pub cc: String,
    pub bcc: String,
    pub reply_to: String,
    pub subject: String,
    pub body: String,
    pub date: i64,
    pub message_id: String,
    pub references: String,
    pub labels: Vec<String>,
    pub files: Vec<(usize, String)>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Draft {
    pub id: i64,
    pub account: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formatting: Vec<crate::compose_format::Span>,
    pub files: Vec<DraftFile>,
    pub thread: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DraftFile {
    pub name: String,
    pub source: DraftFileSource,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum DraftFileSource {
    Staged(i64),
    Cached {
        account: String,
        message: String,
        index: usize,
    },
}

#[derive(Clone, Debug)]
pub struct Outgoing {
    pub id: i64,
    pub account: String,
    pub subject: String,
    pub due: i64,
    pub status: String,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Action {
    Labels {
        add: Vec<String>,
        remove: Vec<String>,
    },
    Trash,
    Restore,
}

impl Action {
    pub fn changes_mailbox(&self) -> bool {
        match self {
            Self::Labels { add, remove } => add
                .iter()
                .chain(remove)
                .any(|label| matches!(label.as_str(), "INBOX" | "SPAM")),
            Self::Trash | Self::Restore => true,
        }
    }

    pub fn inbox_override(&self) -> Option<bool> {
        match self {
            Self::Labels { add, remove } if add.is_empty() && remove == &["INBOX"] => Some(false),
            Self::Labels { add, remove } if remove.is_empty() && add == &["INBOX"] => Some(true),
            _ => None,
        }
    }

    pub fn labels(add: &[&str], remove: &[&str]) -> Self {
        Self::Labels {
            add: add.iter().map(ToString::to_string).collect(),
            remove: remove.iter().map(ToString::to_string).collect(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Filter {
    #[serde(default)]
    pub id: String,
    pub criteria: Criteria,
    pub action: FilterAction,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Criteria {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub from: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub to: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub subject: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub query: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub negated_query: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub has_attachment: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_comparison: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FilterAction {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub add_label_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remove_label_ids: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub forward: String,
}

pub fn date_label(millis: i64, detailed: bool) -> String {
    let Some(time) = Local.timestamp_millis_opt(millis).single() else {
        return String::new();
    };
    if detailed {
        time.format("%a, %b %-d, %Y at %-I:%M %p").to_string()
    } else if time.date_naive() == Local::now().date_naive() {
        time.format("%-I:%M %p").to_string()
    } else {
        time.format("%b %-d").to_string()
    }
}
