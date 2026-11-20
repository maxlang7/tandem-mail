use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::model::{Account, Action, Filter, Label, RemoteMessage};

const BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

#[derive(Debug)]
pub struct ApiError(StatusCode, String);

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gmail {}: {}", self.0.as_u16(), self.1)
    }
}
impl std::error::Error for ApiError {}

pub fn bad_request(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ApiError>()
        .is_some_and(|e| e.0 == StatusCode::BAD_REQUEST)
}

pub fn missing(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ApiError>()
        .is_some_and(|e| e.0 == StatusCode::NOT_FOUND)
}

pub const QUOTA_PAUSE: Duration = Duration::from_secs(65);

#[derive(Debug)]
struct QuotaPause;

impl std::fmt::Display for QuotaPause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Gmail rate limit reached. Retrying in about a minute.")
    }
}
impl std::error::Error for QuotaPause {}

pub fn rate_limited(err: &anyhow::Error) -> bool {
    err.is::<QuotaPause>()
        || err.downcast_ref::<ApiError>().is_some_and(|error| {
            let text = error.1.to_ascii_lowercase();
            error.0 == StatusCode::TOO_MANY_REQUESTS
                || (error.0 == StatusCode::FORBIDDEN
                    && (text.contains("quota exceeded") || text.contains("rate limit")))
        })
}

pub fn retryable_change(err: &anyhow::Error) -> bool {
    rate_limited(err)
        || err.is::<reqwest::Error>()
        || err.downcast_ref::<ApiError>().is_some_and(|error| {
            error.0.is_server_error() || error.0 == StatusCode::REQUEST_TIMEOUT
        })
}

#[derive(Deserialize)]
struct Credentials {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    token_uri: Option<String>,
}

#[derive(Deserialize)]
struct Token {
    access_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub email_address: String,
    pub history_id: String,
    pub messages_total: u64,
}

#[derive(Deserialize)]
pub struct RemoteId {
    pub id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    #[serde(default)]
    pub messages: Vec<RemoteId>,
    pub next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPage {
    #[serde(default)]
    pub history: Vec<History>,
    pub next_page_token: Option<String>,
    pub history_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct History {
    #[serde(default)]
    pub messages: Vec<RemoteId>,
    #[serde(default)]
    pub messages_added: Vec<HistoryChange>,
    #[serde(default)]
    pub messages_deleted: Vec<HistoryChange>,
    #[serde(default)]
    pub labels_added: Vec<HistoryChange>,
    #[serde(default)]
    pub labels_removed: Vec<HistoryChange>,
}

#[derive(Deserialize)]
pub struct HistoryChange {
    pub message: RemoteId,
}

#[derive(Deserialize)]
struct Labels {
    #[serde(default)]
    labels: Vec<Label>,
}

#[derive(Deserialize)]
struct Filters {
    #[serde(default)]
    filter: Vec<Filter>,
}

#[derive(Deserialize)]
struct Thread {
    #[serde(default)]
    messages: Vec<RemoteMessage>,
}

pub struct Gmail {
    client: Client,
    fallback: Client,
    account: Account,
    token: Mutex<Option<(String, Instant)>>,
    quota_until: Mutex<Option<Instant>>,
}

impl Gmail {
    pub fn new(account: Account) -> Result<Self> {
        let client = Client::builder()
            .http1_only()
            .user_agent(concat!("TandemMail/", env!("CARGO_PKG_VERSION"), " (gzip)"))
            .timeout(Duration::from_secs(50))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let fallback = Client::builder()
            .http1_only()
            .no_gzip()
            .timeout(Duration::from_secs(70))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            fallback,
            account,
            token: Mutex::new(None),
            quota_until: Mutex::new(None),
        })
    }

    async fn authenticate(&self, rejected: Option<&str>) -> Result<String> {
        let mut token = self.token.lock().await;
        if let Some((current, expiry)) = token.as_ref()
            && *expiry > Instant::now() + Duration::from_secs(45)
            && rejected != Some(current.as_str())
        {
            return Ok(current.clone());
        }
        let credentials: Credentials =
            serde_json::from_slice(&read_credentials(&self.account.credentials)?)?;
        const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
        ensure!(
            matches!(
                credentials.token_uri.as_deref(),
                None | Some(TOKEN_URL) | Some("https://accounts.google.com/o/oauth2/token")
            ),
            "Unsupported Google token endpoint"
        );
        let response = self
            .client
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", credentials.client_id.as_str()),
                ("client_secret", credentials.client_secret.as_str()),
                ("refresh_token", credentials.refresh_token.as_str()),
            ])
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            anyhow::bail!(
                "Google authorization failed for {} ({}). Renew the local Workspace sign-in if access was revoked.",
                self.account.email,
                status.as_u16()
            );
        }
        let new: Token = response.json().await?;
        let profile = self
            .client
            .get(format!("{BASE}/profile"))
            .bearer_auth(&new.access_token)
            .send()
            .await?;
        let profile: Profile = response_json(profile).await?;
        ensure!(
            profile
                .email_address
                .eq_ignore_ascii_case(&self.account.email),
            "Google authorized a different account than {}",
            self.account.email
        );
        *token = Some((
            new.access_token.clone(),
            Instant::now() + Duration::from_secs(new.expires_in),
        ));
        Ok(new.access_token)
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<T> {
        let mut token = self.authenticate(None).await?;
        let mut renewed = false;
        let mut read_retries = 0;
        loop {
            if self
                .quota_until
                .lock()
                .await
                .is_some_and(|until| Instant::now() < until)
            {
                return Err(QuotaPause.into());
            }
            let client = if read_retries == 0 {
                &self.client
            } else {
                &self.fallback
            };
            let mut req = client
                .request(method.clone(), format!("{BASE}{path}"))
                .bearer_auth(&token)
                .query(query);
            if let Some(body) = &body {
                req = req.json(body);
            }
            let result = match req.send().await {
                Ok(response) if response.status() == StatusCode::UNAUTHORIZED && !renewed => {
                    token = self.authenticate(Some(&token)).await?;
                    renewed = true;
                    continue;
                }
                Ok(response) => response_json(response).await,
                Err(error) => Err(error.without_url().into()),
            };
            if result.as_ref().is_err_and(rate_limited) {
                *self.quota_until.lock().await = Some(Instant::now() + QUOTA_PAUSE);
                return Err(QuotaPause.into());
            }
            if method == Method::GET
                && read_retries < 2
                && result.as_ref().is_err_and(|error| {
                    error
                        .downcast_ref::<ApiError>()
                        .is_some_and(|error| error.0.is_server_error())
                        || error.is::<reqwest::Error>()
                })
            {
                tokio::time::sleep(Duration::from_secs(1 << read_retries)).await;
                read_retries += 1;
                continue;
            }
            return result;
        }
    }

    pub async fn ready_to_send(&self) -> Result<()> {
        self.profile().await.map(|_| ())
    }

    pub async fn profile(&self) -> Result<Profile> {
        self.request(Method::GET, "/profile", &[], None).await
    }

    pub async fn label_list(&self) -> Result<Vec<Label>> {
        Ok(self
            .request::<Labels>(Method::GET, "/labels", &[], None)
            .await?
            .labels)
    }

    pub async fn message_list(&self, token: Option<&str>, query: Option<&str>) -> Result<Listing> {
        let mut params = vec![("maxResults", "60"), ("includeSpamTrash", "true")];
        if let Some(token) = token {
            params.push(("pageToken", token));
        }
        if let Some(query) = query {
            params.push(("q", query));
        }
        self.request(Method::GET, "/messages", &params, None).await
    }

    pub async fn message(&self, id: &str, raw: bool) -> Result<RemoteMessage> {
        self.request(
            Method::GET,
            &format!("/messages/{}", segment(id)?),
            &[("format", if raw { "raw" } else { "minimal" })],
            None,
        )
        .await
    }

    pub async fn history(&self, start: &str, token: Option<&str>) -> Result<HistoryPage> {
        let mut params = vec![("startHistoryId", start), ("maxResults", "100")];
        if let Some(token) = token {
            params.push(("pageToken", token));
        }
        self.request(Method::GET, "/history", &params, None).await
    }

    pub async fn change_thread(&self, id: &str, action: &Action) -> Result<Vec<RemoteMessage>> {
        let (verb, body) = match action {
            Action::Labels { add, remove } => (
                "modify",
                Some(json!({"addLabelIds": add, "removeLabelIds": remove})),
            ),
            Action::Trash => ("trash", None),
            Action::Restore => ("untrash", None),
        };
        let thread: Thread = self
            .request(
                Method::POST,
                &format!("/threads/{}/{verb}", segment(id)?),
                &[("fields", "messages(id,threadId,labelIds)")],
                body,
            )
            .await?;
        Ok(thread.messages)
    }

    pub async fn create_label(&self, name: &str) -> Result<Label> {
        self.request(Method::POST, "/labels", &[], Some(json!({"name": name})))
            .await
    }

    pub async fn filters(&self) -> Result<Vec<Filter>> {
        Ok(self
            .request::<Filters>(Method::GET, "/settings/filters", &[], None)
            .await?
            .filter)
    }

    pub async fn create_filter(&self, filter: &Filter) -> Result<Filter> {
        self.request(
            Method::POST,
            "/settings/filters",
            &[],
            Some(json!({"criteria": filter.criteria, "action": filter.action})),
        )
        .await
    }

    pub async fn delete_filter(&self, id: &str) -> Result<()> {
        self.request(
            Method::DELETE,
            &format!("/settings/filters/{}", segment(id)?),
            &[],
            None,
        )
        .await
    }

    pub async fn send(&self, raw: &str, thread: Option<&str>) -> Result<RemoteId> {
        let mut message = json!({"raw": raw});
        if let Some(thread) = thread {
            message["threadId"] = json!(thread);
        }
        self.request(Method::POST, "/messages/send", &[], Some(message))
            .await
    }
}

fn segment(id: &str) -> Result<&str> {
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "Invalid Gmail identifier"
    );
    Ok(id)
}

fn read_credentials(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path)
        .with_context(|| format!("No local Gmail authorization at {}", path.display()))
}

async fn response_json<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    if status == StatusCode::NO_CONTENT {
        return Ok(serde_json::from_str("null")?);
    }
    if status.is_success() {
        return Ok(response.json().await.map_err(reqwest::Error::without_url)?);
    }
    let error: Value = response.json().await.unwrap_or_default();
    Err(ApiError(
        status,
        error["error"]["message"]
            .as_str()
            .unwrap_or("Request failed")
            .to_owned(),
    )
    .into())
}
