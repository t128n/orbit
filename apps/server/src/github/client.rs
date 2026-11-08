//! GitHub App REST client for issue sync.
//!
//! The App JWT is signed with the RSA key stored encrypted under the server app key, and
//! installation tokens are cached in memory only. No request or response body is logged.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, USER_AGENT};
use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::secret_box::decrypt_secret;

pub const DEFAULT_API_BASE: &str = "https://api.github.com/";
const API_VERSION: &str = "2022-11-28";
const USER_AGENT_VALUE: &str = "Orbit";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Installation tokens live for one hour; refresh with a margin instead of at expiry.
const INSTALLATION_TOKEN_TTL: Duration = Duration::from_secs(50 * 60);

/// GitHub App credentials as stored in `github_apps`.
#[derive(Clone, Debug)]
pub struct AppCredentials {
    pub app_id: i64,
    /// ChaCha20-Poly1305 ciphertext under the server app key.
    pub private_key_encrypted: Vec<u8>,
}

/// A GitHub issue. Pull requests are returned by the issues API with `pull_request` set.
#[derive(Clone, Debug, Deserialize)]
pub struct Issue {
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub state_reason: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub labels: Vec<IssueLabel>,
    pub updated_at: Option<String>,
    #[serde(default)]
    pub pull_request: Option<Value>,
}

impl Issue {
    #[must_use]
    pub fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }

    #[must_use]
    pub fn labels(&self) -> impl Iterator<Item = &str> {
        self.labels.iter().map(|label| label.name.as_str())
    }

    #[must_use]
    pub fn body(&self) -> &str {
        self.body.as_deref().unwrap_or("")
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct IssueLabel {
    pub name: String,
}

/// One page of repository issues and the `Link: rel="next"` target, if any.
#[derive(Clone, Debug)]
pub struct IssuePage {
    pub issues: Vec<Issue>,
    pub next: Option<String>,
}

#[derive(Debug, Error)]
pub enum GithubError {
    #[error("the server app key is not configured")]
    AppKeyMissing,
    #[error("the GitHub App private key cannot be read")]
    InvalidCredentials,
    #[error("GitHub rejected the App credentials")]
    Unauthorized,
    #[error("the GitHub App needs Issues write permission")]
    Permission,
    #[error("the GitHub rate limit was reached")]
    RateLimited { retry_after: Option<Duration> },
    #[error("the GitHub issue was not found")]
    NotFound,
    #[error("GitHub rejected the issue: {0}")]
    Validation(String),
    #[error("GitHub is unavailable")]
    Server,
    #[error("the GitHub request failed")]
    Network,
    #[error("the GitHub response is invalid")]
    InvalidResponse,
}

impl GithubError {
    /// Whether the job retry policy should try again.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::Server | Self::Network
        )
    }

    /// How long to wait when GitHub reported a rate limit.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            _ => None,
        }
    }

    /// A safe message for `github_issue_links.sync_error`; it never includes content.
    #[must_use]
    pub fn sync_message(&self) -> String {
        match self {
            Self::AppKeyMissing => "The server app key is not configured.".to_owned(),
            Self::InvalidCredentials => "The GitHub App private key cannot be read.".to_owned(),
            Self::Unauthorized => "The GitHub App credentials are invalid.".to_owned(),
            Self::Permission => "The GitHub App needs Issues write permission.".to_owned(),
            Self::RateLimited { .. } => {
                "GitHub is rate limiting requests; Orbit retries later.".to_owned()
            }
            Self::NotFound => "The GitHub issue no longer exists or is not accessible.".to_owned(),
            Self::Validation(message) => format!("GitHub rejected the issue: {message}"),
            Self::Server => "GitHub is unavailable; Orbit retries later.".to_owned(),
            Self::Network => "GitHub could not be reached; Orbit retries later.".to_owned(),
            Self::InvalidResponse => "GitHub returned an unexpected response.".to_owned(),
        }
    }
}

#[derive(Clone, Debug)]
struct CachedToken {
    token: String,
    expires_at: Instant,
}

#[derive(Clone)]
pub struct GithubClient {
    http: reqwest::Client,
    base: Url,
    app_key: Option<[u8; 32]>,
    tokens: Arc<Mutex<HashMap<(i64, i64), CachedToken>>>,
}

impl Default for GithubClient {
    fn default() -> Self {
        Self::new()
    }
}

impl GithubClient {
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_API_BASE).expect("the default GitHub API URL is valid")
    }

    /// Builds a client for `base` (the default is [`DEFAULT_API_BASE`]). Tests point this at a
    /// local fake server.
    pub fn with_base_url(base: &str) -> Result<Self, GithubError> {
        let mut base = Url::parse(base).map_err(|_| GithubError::InvalidResponse)?;
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| GithubError::Network)?;
        Ok(Self {
            http,
            base,
            app_key: None,
            tokens: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    #[must_use]
    pub fn with_app_key(mut self, app_key: Option<[u8; 32]>) -> Self {
        self.app_key = app_key;
        self
    }

    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// The first repository issues page URL. Labelled mode filters by `label`; all-issues mode
    /// passes `None`. Closed issues are fetched only when `include_closed` is set.
    pub fn repository_issues_url(
        &self,
        repository: &str,
        label: Option<&str>,
        include_closed: bool,
    ) -> Result<Url, GithubError> {
        let (owner, name) = repository_parts(repository)?;
        let mut url = self
            .base
            .join(&format!("repos/{owner}/{name}/issues"))
            .map_err(|_| GithubError::InvalidResponse)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("per_page", "100");
            query.append_pair("state", if include_closed { "all" } else { "open" });
            if let Some(label) = label.filter(|label| !label.is_empty()) {
                query.append_pair("labels", label);
            }
        }
        Ok(url)
    }

    /// Creates an issue and returns GitHub's representation of it.
    pub async fn create_issue(
        &self,
        app: &AppCredentials,
        installation_id: i64,
        repository: &str,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<Issue, GithubError> {
        let token = self.installation_token(app, installation_id).await?;
        let (owner, name) = repository_parts(repository)?;
        let url = self
            .base
            .join(&format!("repos/{owner}/{name}/issues"))
            .map_err(|_| GithubError::InvalidResponse)?;
        let mut payload = json!({ "title": title, "body": body });
        if !labels.is_empty() {
            payload["labels"] = json!(labels);
        }
        let response = self
            .send(Method::POST, url, Some(&token), Some(&payload))
            .await?;
        decode_json(&response.body)
    }

    /// Patches an issue's title, body and state. `state_reason` is sent only when GitHub expects
    /// it (a closed issue).
    pub async fn update_issue(
        &self,
        app: &AppCredentials,
        installation_id: i64,
        repository: &str,
        number: i64,
        title: &str,
        body: &str,
        state: &str,
        state_reason: Option<&str>,
    ) -> Result<Issue, GithubError> {
        let token = self.installation_token(app, installation_id).await?;
        let (owner, name) = repository_parts(repository)?;
        let url = self
            .base
            .join(&format!("repos/{owner}/{name}/issues/{number}"))
            .map_err(|_| GithubError::InvalidResponse)?;
        let mut payload = json!({ "title": title, "body": body, "state": state });
        if let Some(reason) = state_reason {
            payload["state_reason"] = json!(reason);
        }
        let response = self
            .send(Method::PATCH, url, Some(&token), Some(&payload))
            .await?;
        decode_json(&response.body)
    }

    pub async fn get_issue(
        &self,
        app: &AppCredentials,
        installation_id: i64,
        repository: &str,
        number: i64,
    ) -> Result<Issue, GithubError> {
        let token = self.installation_token(app, installation_id).await?;
        let (owner, name) = repository_parts(repository)?;
        let url = self
            .base
            .join(&format!("repos/{owner}/{name}/issues/{number}"))
            .map_err(|_| GithubError::InvalidResponse)?;
        let response = self.send(Method::GET, url, Some(&token), None).await?;
        decode_json(&response.body)
    }

    /// Fetches one page of issues. `url` must point at the same origin as the API base, so a
    /// crafted `Link` header cannot receive the installation token.
    pub async fn list_issues_page(
        &self,
        app: &AppCredentials,
        installation_id: i64,
        url: &str,
    ) -> Result<IssuePage, GithubError> {
        let url = Url::parse(url).map_err(|_| GithubError::InvalidResponse)?;
        if url.origin() != self.base.origin() {
            return Err(GithubError::InvalidResponse);
        }
        let token = self.installation_token(app, installation_id).await?;
        let response = self
            .send(Method::GET, url.clone(), Some(&token), None)
            .await?;
        let issues: Vec<Issue> = decode_json(&response.body)?;
        let next = next_link(&response.headers, &url)?;
        Ok(IssuePage { issues, next })
    }

    /// Ensures a label exists. A label that already exists is a success.
    pub async fn ensure_label(
        &self,
        app: &AppCredentials,
        installation_id: i64,
        repository: &str,
        label: &str,
    ) -> Result<(), GithubError> {
        let token = self.installation_token(app, installation_id).await?;
        let (owner, name) = repository_parts(repository)?;
        let encoded = utf8_percent_encode(label, NON_ALPHANUMERIC).to_string();
        let url = self
            .base
            .join(&format!("repos/{owner}/{name}/labels/{encoded}"))
            .map_err(|_| GithubError::InvalidResponse)?;
        match self.send(Method::GET, url, Some(&token), None).await {
            Ok(_) => return Ok(()),
            Err(GithubError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let url = self
            .base
            .join(&format!("repos/{owner}/{name}/labels"))
            .map_err(|_| GithubError::InvalidResponse)?;
        let payload = json!({ "name": label, "color": "ededed" });
        match self
            .send(Method::POST, url, Some(&token), Some(&payload))
            .await
        {
            // GitHub answers 422 when the label already exists (or the name is invalid).
            Ok(_) | Err(GithubError::Validation(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Returns an installation token, reusing the cached one until it is near expiry.
    pub async fn installation_token(
        &self,
        app: &AppCredentials,
        installation_id: i64,
    ) -> Result<String, GithubError> {
        let cache_key = (app.app_id, installation_id);
        if let Ok(tokens) = self.tokens.lock()
            && let Some(cached) = tokens.get(&cache_key)
            && cached.expires_at > Instant::now()
        {
            return Ok(cached.token.clone());
        }
        let jwt = self.app_jwt(app)?;
        let url = self
            .base
            .join(&format!(
                "app/installations/{installation_id}/access_tokens"
            ))
            .map_err(|_| GithubError::InvalidResponse)?;
        let response = self.send(Method::POST, url, Some(&jwt), None).await?;
        let token: InstallationToken = decode_json(&response.body)?;
        if token.token.is_empty() {
            return Err(GithubError::InvalidCredentials);
        }
        if let Ok(mut tokens) = self.tokens.lock() {
            tokens.insert(
                cache_key,
                CachedToken {
                    token: token.token.clone(),
                    expires_at: Instant::now() + INSTALLATION_TOKEN_TTL,
                },
            );
        }
        Ok(token.token)
    }

    fn app_jwt(&self, app: &AppCredentials) -> Result<String, GithubError> {
        let key = self.app_key.as_ref().ok_or(GithubError::AppKeyMissing)?;
        let pem = decrypt_secret(key, &app.private_key_encrypted)
            .map_err(|_| GithubError::InvalidCredentials)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| GithubError::InvalidCredentials)?
            .as_secs();
        let claims = Claims {
            iat: now.saturating_sub(60),
            exp: now + 540,
            iss: app.app_id.to_string(),
        };
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).map_err(|_| GithubError::InvalidCredentials)?);
        let signing = format!("{header}.{payload}");

        use rsa::pkcs1::DecodeRsaPrivateKey;
        use rsa::pkcs8::DecodePrivateKey;
        use rsa::signature::{RandomizedSigner, SignatureEncoding};
        let private = rsa::RsaPrivateKey::from_pkcs1_pem(&pem)
            .or_else(|_| rsa::RsaPrivateKey::from_pkcs8_pem(&pem))
            .map_err(|_| GithubError::InvalidCredentials)?;
        let signer = rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new(private);
        let mut rng = rand_core::OsRng;
        let signature = signer.sign_with_rng(&mut rng, signing.as_bytes());
        Ok(format!(
            "{signing}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        ))
    }

    async fn send(
        &self,
        method: Method,
        url: Url,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Result<GithubResponse, GithubError> {
        if url.origin() != self.base.origin() {
            return Err(GithubError::InvalidResponse);
        }
        let mut request = self
            .http
            .request(method, url)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(USER_AGENT, USER_AGENT_VALUE);
        if let Some(token) = token {
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|_| GithubError::Network)?;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.map_err(|_| GithubError::Network)?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(GithubError::InvalidResponse);
        }
        classify(status, &headers, &bytes)?;
        Ok(GithubResponse {
            body: bytes.to_vec(),
            headers,
        })
    }
}

#[derive(Debug)]
struct GithubResponse {
    body: Vec<u8>,
    headers: HeaderMap,
}

#[derive(Deserialize)]
struct InstallationToken {
    token: String,
}

#[derive(Serialize)]
struct Claims {
    iat: u64,
    exp: u64,
    iss: String,
}

fn classify(status: StatusCode, headers: &HeaderMap, body: &[u8]) -> Result<(), GithubError> {
    if status.is_success() {
        return Ok(());
    }
    let message = json_message(body).unwrap_or_default();
    match status.as_u16() {
        401 => Err(GithubError::Unauthorized),
        403 => {
            let remaining = headers
                .get("x-ratelimit-remaining")
                .and_then(|value| value.to_str().ok());
            if remaining == Some("0") || message.to_ascii_lowercase().contains("rate limit") {
                Err(GithubError::RateLimited {
                    retry_after: rate_limit_delay(headers),
                })
            } else {
                Err(GithubError::Permission)
            }
        }
        404 | 410 => Err(GithubError::NotFound),
        422 => Err(GithubError::Validation(validation_message(body))),
        429 => Err(GithubError::RateLimited {
            retry_after: rate_limit_delay(headers),
        }),
        500..=599 => Err(GithubError::Server),
        _ => Err(GithubError::InvalidResponse),
    }
}

fn rate_limit_delay(headers: &HeaderMap) -> Option<Duration> {
    if let Some(seconds) = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Some(Duration::from_secs(seconds));
    }
    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(reset.saturating_sub(now)))
}

fn json_message(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value.get("message")?.as_str().map(str::to_owned)
}

/// A safe 422 summary: field names and codes, never the submitted title or body.
fn validation_message(body: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return "validation failed".to_owned();
    };
    let mut parts = Vec::new();
    if let Some(errors) = value.get("errors").and_then(Value::as_array) {
        for error in errors {
            let field = error.get("field").and_then(Value::as_str);
            let code = error.get("code").and_then(Value::as_str);
            match (field, code) {
                (Some(field), Some(code)) => parts.push(format!("{field} {code}")),
                (Some(field), None) => parts.push(field.to_owned()),
                (None, Some(code)) => parts.push(code.to_owned()),
                (None, None) => {}
            }
        }
    }
    if parts.is_empty() {
        value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("validation failed")
            .chars()
            .take(200)
            .collect()
    } else {
        parts.join(", ").chars().take(200).collect()
    }
}

fn decode_json<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, GithubError> {
    serde_json::from_slice(body).map_err(|_| GithubError::InvalidResponse)
}

fn next_link(headers: &HeaderMap, requested: &Url) -> Result<Option<String>, GithubError> {
    let mut next = None;
    for value in headers.get_all("link") {
        let links = value.to_str().map_err(|_| GithubError::InvalidResponse)?;
        for part in links.split(',') {
            let (target, params) = part
                .trim()
                .split_once(';')
                .ok_or(GithubError::InvalidResponse)?;
            let target = target
                .trim()
                .strip_prefix('<')
                .and_then(|target| target.strip_suffix('>'))
                .ok_or(GithubError::InvalidResponse)?;
            let is_next = params.split(';').map(str::trim).any(|param| {
                param
                    .strip_prefix("rel=")
                    .map(|rel| {
                        rel.trim_matches('"')
                            .split_whitespace()
                            .any(|rel| rel == "next")
                    })
                    .unwrap_or(false)
            });
            if is_next {
                if next.is_some() {
                    return Err(GithubError::InvalidResponse);
                }
                next = Some(
                    requested
                        .join(target)
                        .map_err(|_| GithubError::InvalidResponse)?
                        .to_string(),
                );
            }
        }
    }
    Ok(next)
}

fn repository_parts(repository: &str) -> Result<(&str, &str), GithubError> {
    let (owner, name) = repository
        .split_once('/')
        .ok_or(GithubError::InvalidResponse)?;
    if !valid_repository_part(owner) || !valid_repository_part(name) || name.contains('/') {
        return Err(GithubError::InvalidResponse);
    }
    Ok((owner, name))
}

fn valid_repository_part(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 100
        && part != "."
        && part != ".."
        && part.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

#[cfg(test)]
mod tests {
    use super::{GithubClient, GithubError, next_link, repository_parts};
    use reqwest::Url;

    #[test]
    fn repository_names_must_be_owner_and_name() {
        assert_eq!(repository_parts("owner/repo").unwrap(), ("owner", "repo"));
        assert!(repository_parts("owner").is_err());
        assert!(repository_parts("owner/repo/extra").is_err());
        assert!(repository_parts("owner/../repo").is_err());
        assert!(repository_parts("owner/r#po").is_err());
    }

    #[test]
    fn the_link_header_yields_the_next_page() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "link",
            "<https://api.github.com/repositories/1/issues?page=2>; rel=\"next\", <https://api.github.com/repositories/1/issues?page=5>; rel=\"last\""
                .parse()
                .unwrap(),
        );
        let requested = Url::parse("https://api.github.com/repositories/1/issues").unwrap();
        assert_eq!(
            next_link(&headers, &requested).unwrap().as_deref(),
            Some("https://api.github.com/repositories/1/issues?page=2")
        );
    }

    #[test]
    fn a_base_url_without_a_trailing_slash_still_joins_paths() {
        let client = GithubClient::with_base_url("http://127.0.0.1:1234").unwrap();
        assert_eq!(client.base_url().as_str(), "http://127.0.0.1:1234/");
    }

    #[test]
    fn rate_limited_errors_are_retryable() {
        let error = GithubError::RateLimited { retry_after: None };
        assert!(error.is_retryable());
        assert!(!GithubError::Permission.is_retryable());
    }
}
