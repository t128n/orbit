//! GitHub App polling primitives. This module proposes effects; it never persists them.
//! Conditional requests use caller-supplied, generation-scoped cached pages.

use std::collections::{HashMap, HashSet};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use orbit_platform::{Database, Id, Row};
use serde::{Deserialize, Serialize};

use crate::repositories::tasks::GithubWorkItem;
use crate::secret_box::decrypt_secret;

pub(crate) mod worker;

/// A configured project/repository/label mapping. The scheduler loads these after discovery.
#[derive(Clone, Debug)]
pub(crate) struct Connection {
    pub installation_id: i64,
    pub repository: String,
    pub label: String,
    pub project_id: Id,
}

/// Snapshot of links for a workspace. Reload for each poll; writes must still be fenced.
#[derive(Clone, Debug)]
pub(crate) struct TrackedItem {
    pub repository: String,
    pub number: i64,
    pub kind: String,
}

pub(crate) async fn load_tracked_items(
    database: &Database,
    workspace: &str,
) -> Result<Vec<TrackedItem>, sqlx::Error> {
    let rows = sqlx::query("SELECT repository, issue_number, kind FROM github_issue_links WHERE workspace_id = $1 AND sync_paused = 0")
        .bind(workspace)
        .fetch_all(database.pool())
        .await?;
    rows.into_iter().map(|row| Ok(TrackedItem {
        repository: row.try_get("repository")?,
        number: row.try_get("issue_number")?,
        kind: row.try_get("kind")?,
    })).collect()
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RateLimit {
    pub remaining: Option<u64>,
    /// Unix seconds, as supplied by GitHub.
    pub reset: Option<u64>,
    /// Delay in seconds. HTTP-date Retry-After is not interpreted.
    pub retry_after: Option<u64>,
}

/// A cached response must include the body AND pagination link, scoped by installation,
/// endpoint, query, and authorization/configuration generation. A 304 is usable only
/// when that exact representation exists; otherwise retry unconditionally. Cache writes
/// must be fenced along with all other poll effects.
#[derive(Clone, Debug)]
pub(crate) struct CachedPage {
    pub etag: String,
    pub body: Vec<u8>,
    pub next: Option<String>,
}

pub(crate) trait PageCache: Sync {
    fn get(&self, scope: &str, url: &str) -> Option<CachedPage>;
    fn put(&mut self, scope: &str, url: &str, page: CachedPage);
}

/// Cache changes are proposals, not writes. Commit only after the poll's effects are
/// applied under the same configuration and lease fence. Scope must identify the
/// workspace, app credentials and configuration/authorization generation.
pub(crate) struct PageUpdate {
    pub scope: String,
    pub url: String,
    /// None invalidates an older representation after a fresh response without an ETag.
    pub page: Option<CachedPage>,
}

const MAX_RESPONSE_BODY: usize = 8 * 1024 * 1024;
const MAX_REPOSITORY_BODY: usize = 32 * 1024 * 1024;
const MAX_POLL_PROPOSAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_POLL_PROPOSALS: usize = 512;
const MAX_REPOSITORY_PAGES: usize = 512;
const MAX_DISCOVERY_PAGES: usize = 512;
const MAX_DISCOVERY_BODY: usize = 32 * 1024 * 1024;
const MAX_DISCOVERY_INSTALLATIONS: usize = 1024;
const MAX_INSTALLATION_TOKEN: usize = 8192;

#[derive(Default)]
struct DiscoveryBudget {
    pages: usize,
    body_bytes: usize,
}

impl DiscoveryBudget {
    fn start_page(&mut self) -> Result<(), String> {
        if self.pages >= MAX_DISCOVERY_PAGES {
            return Err("GitHub discovery page limit exceeded".into());
        }
        self.pages += 1;
        Ok(())
    }

    fn add_body(&mut self, size: usize) -> Result<(), String> {
        self.body_bytes = self.body_bytes.saturating_add(size);
        if self.body_bytes > MAX_DISCOVERY_BODY {
            return Err("GitHub discovery response limit exceeded".into());
        }
        Ok(())
    }
}

pub(crate) struct PollOptions<'a> {
    pub cache: Option<&'a dyn PageCache>,
    pub cache_scope: &'a str,
    /// GitHub ISO 8601 timestamp. Use an overlap and advance only after all effects
    /// succeed. A delta is not a complete reconciliation or mode handoff.
    pub since: Option<&'a str>,
}

#[derive(Debug)]
pub(crate) struct PollFailure {
    pub message: String,
    pub rate_limit: RateLimit,
}

/// Apply only after revalidating billing, selection, configuration and the lease in the
/// same immediate transaction as each write. Existing repository methods open their own
/// transactions and therefore cannot safely apply these actions without refactoring.
pub(crate) enum PollAction {
    Observe(GithubWorkItem),
    Pause {
        repository: String,
        number: i64,
        kind: &'static str,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct RepositoryError {
    pub repository: String,
    pub message: String,
}

#[derive(Default)]
pub(crate) struct PollOutcome {
    pub installations: Vec<Installation>,
    pub repositories: Vec<InstalledRepository>,
    pub items_seen: u64,
    pub errors: Vec<RepositoryError>,
    pub rate_limit: RateLimit,
    pub actions: Vec<PollAction>,
    pub page_updates: Vec<PageUpdate>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Installation {
    pub id: i64,
    pub account: Account,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Account {
    pub login: String,
}

#[derive(Clone, Debug)]
pub(crate) struct InstalledRepository {
    pub installation_id: i64,
    pub full_name: String,
}

#[derive(Deserialize)]
struct RepoPage {
    repositories: Vec<Repo>,
}
#[derive(Deserialize)]
struct Repo {
    full_name: String,
}
#[derive(Deserialize)]
struct Token {
    token: String,
}
#[derive(Deserialize)]
struct Label {
    name: String,
}
#[derive(Deserialize)]
struct Item {
    number: i64,
    title: String,
    body: Option<String>,
    state: String,
    labels: Vec<Label>,
    pull_request: Option<serde_json::Value>,
    repository_url: Option<String>,
}
#[derive(Deserialize)]
struct Pull {
    merged_at: Option<String>,
}

struct Api {
    client: reqwest::Client,
    base: reqwest::Url,
    rate: RateLimit,
}

impl Api {
    async fn request<T: serde::de::DeserializeOwned>(
        &mut self,
        method: reqwest::Method,
        url: reqwest::Url,
        token: &str,
    ) -> Result<(T, Option<reqwest::Url>), String> {
        let (value, next, _, _) = self.request_page(method, url, token, None).await?;
        Ok((value, next))
    }

    async fn request_page<T: serde::de::DeserializeOwned>(
        &mut self,
        method: reqwest::Method,
        url: reqwest::Url,
        token: &str,
        cached: Option<CachedPage>,
    ) -> Result<(T, Option<reqwest::Url>, Option<Option<CachedPage>>, usize), String> {
        // Never forward installation or app credentials to a Link target on another origin.
        if url.origin() != self.base.origin() {
            return Err("GitHub pagination changed origin".into());
        }
        let requested_url = url.clone();
        // Reject unusable cached representations before sending a conditional request.
        let cached = cached.filter(|page| {
            page.body.len() <= MAX_RESPONSE_BODY
                && !page.etag.is_empty()
                && reqwest::header::HeaderValue::from_str(&page.etag).is_ok()
                && serde_json::from_slice::<T>(&page.body).is_ok()
                && page.next.as_ref().is_none_or(|next| {
                    url.join(next).is_ok_and(|next| next.origin() == self.base.origin())
                })
        });
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "orbit-github-poller");
        if let Some(page) = &cached {
            request = request.header(reqwest::header::IF_NONE_MATCH, &page.etag);
        }
        let mut response = request.send()
            .await
            .map_err(|_| "GitHub request failed".to_owned())?;
        if response.url() != &requested_url {
            return Err("GitHub request redirected".into());
        }
        let headers = response.headers();
        let number = |name: &str| {
            headers
                .get(name)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse().ok())
        };
        if let Some(remaining) = number("x-ratelimit-remaining") {
            // Keep the most restrictive response instead of hiding a previously exhausted bucket.
            if self.rate.remaining.is_none_or(|old| remaining < old) {
                self.rate.remaining = Some(remaining);
                self.rate.reset = number("x-ratelimit-reset");
            } else if self.rate.remaining == Some(remaining) {
                if let Some(reset) = number("x-ratelimit-reset") {
                    self.rate.reset = Some(self.rate.reset.unwrap_or(0).max(reset));
                }
            }
        }
        if let Some(retry) = number("retry-after") {
            self.rate.retry_after = Some(self.rate.retry_after.unwrap_or(0).max(retry));
        }
        let status = response.status();
        if status == reqwest::StatusCode::NOT_MODIFIED {
            let page = cached.ok_or("GitHub returned 304 without a cached page")?;
            let value = serde_json::from_slice(&page.body)
                .map_err(|_| "invalid cached GitHub response")?;
            let next = page.next.as_deref().map(|next| requested_url.join(next))
                .transpose().map_err(|_| "invalid cached GitHub pagination link")?;
            return Ok((value, next, None, page.body.len()));
        }
        if !status.is_success() {
            return Err(format!("GitHub HTTP {status}"));
        }
        let mut next = None;
        for value in headers.get_all("link") {
            let links = value
                .to_str()
                .map_err(|_| "invalid GitHub pagination link")?;
            for part in links.split(',') {
                let (target, params) = part
                    .trim()
                    .split_once(';')
                    .ok_or("invalid GitHub pagination link")?;
                let target = target
                    .trim()
                    .strip_prefix('<')
                    .and_then(|v| v.strip_suffix('>'))
                    .ok_or("invalid GitHub pagination link")?;
                let rel = params
                    .split(';')
                    .map(str::trim)
                    .find_map(|p| p.strip_prefix("rel="));
                if rel.is_some_and(|r| r.trim_matches('"').split_whitespace().any(|v| v == "next"))
                {
                    if next.is_some() {
                        return Err("ambiguous GitHub pagination link".into());
                    }
                    next = Some(
                        requested_url
                            .join(target)
                            .map_err(|_| "invalid GitHub pagination link")?,
                    );
                }
            }
        }
        let etag = headers.get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok()).map(str::to_owned);
        if response.content_length().is_some_and(|length| length > MAX_RESPONSE_BODY as u64) {
            return Err("GitHub response exceeds size limit".into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "invalid GitHub response".to_owned())? {
            if chunk.len() > MAX_RESPONSE_BODY - body.len() {
                return Err("GitHub response exceeds size limit".into());
            }
            body.extend_from_slice(&chunk);
        }
        let value = serde_json::from_slice(&body)
            .map_err(|_| "invalid GitHub response".to_owned())?;
        let body_len = body.len();
        let update = etag.filter(|etag| !etag.is_empty()).map(|etag| CachedPage {
            etag, body, next: next.as_ref().map(ToString::to_string),
        });
        Ok((value, next, Some(update), body_len))
    }

    fn url(&self, path: &str) -> Result<reqwest::Url, String> {
        self.base
            .join(path)
            .map_err(|_| "invalid GitHub URL".into())
    }

    async fn pages<T: serde::de::DeserializeOwned>(
        &mut self,
        path: &str,
        token: &str,
        budget: &mut DiscoveryBudget,
    ) -> Result<Vec<T>, String> {
        let mut next = Some(self.url(path)?);
        let mut visited = HashSet::new();
        let mut result = Vec::new();
        while let Some(url) = next {
            budget.start_page()?;
            if !visited.insert(url.to_string()) {
                return Err("GitHub pagination loop".into());
            }
            let (mut page, link, _, size): (Vec<T>, _, _, _) =
                self.request_page(reqwest::Method::GET, url, token, None).await?;
            budget.add_body(size)?;
            if page.len() > MAX_DISCOVERY_INSTALLATIONS.saturating_sub(result.len()) {
                return Err("GitHub installation limit exceeded".into());
            }
            result.append(&mut page);
            next = link;
        }
        Ok(result)
    }

    async fn repositories(&mut self, token: &str, budget: &mut DiscoveryBudget) -> Result<Vec<Repo>, String> {
        let mut next = Some(self.url("/installation/repositories?per_page=100")?);
        let mut visited = HashSet::new();
        let mut result = Vec::new();
        while let Some(url) = next {
            budget.start_page()?;
            if !visited.insert(url.to_string()) {
                return Err("GitHub pagination loop".into());
            }
            let (page, link, _, size): (RepoPage, _, _, _) =
                self.request_page(reqwest::Method::GET, url, token, None).await?;
            budget.add_body(size)?;
            result.extend(page.repositories);
            next = link;
        }
        Ok(result)
    }

    async fn issue_pages<T: serde::de::DeserializeOwned>(
        &mut self, url: reqwest::Url, token: &str, scope: &str,
        cache: Option<&dyn PageCache>, proposal_count: usize, proposal_bytes: usize,
    ) -> Result<(Vec<T>, Vec<PageUpdate>), String> {
        let mut next = Some(url);
        let mut visited = HashSet::new();
        let mut items = Vec::new();
        let mut updates = Vec::new();
        let mut body_bytes = 0_usize;
        let mut update_bytes = proposal_bytes;
        while let Some(url) = next {
            if visited.len() >= MAX_REPOSITORY_PAGES {
                return Err("GitHub repository page limit exceeded".into());
            }
            let key = url.to_string();
            if !visited.insert(key.clone()) {
                return Err("GitHub pagination loop".into());
            }
            // An unchanged last page can gain a successor without its body changing.
            // Fetch it unconditionally so its current Link header is observed.
            let cached = cache.and_then(|cache| cache.get(scope, &key))
                .filter(|page| page.next.is_some());
            let (mut page, link, update, size): (Vec<T>, _, _, _) = self
                .request_page(reqwest::Method::GET, url, token, cached).await?;
            body_bytes = body_bytes.saturating_add(size);
            if body_bytes > MAX_REPOSITORY_BODY {
                return Err("GitHub repository response limit exceeded".into());
            }
            items.append(&mut page);
            if let (Some(_), Some(page)) = (cache, update) {
                let size = scope.len().saturating_add(key.len()).saturating_add(page.as_ref().map_or(0, |page| {
                    page.etag.len().saturating_add(page.body.len()).saturating_add(page.next.as_ref().map_or(0, String::len))
                }));
                update_bytes = update_bytes.saturating_add(size);
                if proposal_count + updates.len() >= MAX_POLL_PROPOSALS || update_bytes > MAX_POLL_PROPOSAL_BYTES {
                    return Err("GitHub poll cache proposal limit exceeded".into());
                }
                updates.push(PageUpdate { scope: scope.into(), url: key, page });
            }
            next = link;
        }
        Ok((items, updates))
    }
}

#[derive(Serialize)]
struct Claims {
    iat: u64,
    exp: u64,
    iss: String,
}

fn app_jwt(app_id: i64, encrypted_key: &[u8], key: &[u8; 32]) -> Result<String, String> {
    let pem =
        decrypt_secret(key, encrypted_key).map_err(|_| "cannot decrypt GitHub App private key")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "invalid system clock")?
        .as_secs();
    let claims = Claims {
        iat: now.saturating_sub(60),
        exp: now + 540,
        iss: app_id.to_string(),
    };
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).map_err(|_| "cannot encode JWT")?);
    let signing = format!("{header}.{payload}");
    use rsa::pkcs1::DecodeRsaPrivateKey;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::signature::{RandomizedSigner, SignatureEncoding};
    let private = rsa::RsaPrivateKey::from_pkcs1_pem(&pem)
        .or_else(|_| rsa::RsaPrivateKey::from_pkcs8_pem(&pem))
        .map_err(|_| "invalid GitHub App private key")?;
    let signer = rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new(private);
    let signature = signer.sign_with_rng(&mut rsa::rand_core::OsRng, signing.as_bytes());
    Ok(format!(
        "{signing}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

/// Collect one workspace observation, without applying any effects.
/// `base_url` is normally `https://api.github.com/`; override only with a trusted test server.
#[allow(dead_code)]
pub(crate) async fn poll_workspace(
    app_id: i64,
    encrypted_key: &[u8],
    key: &[u8; 32],
    connections: &[Connection],
    tracked: &[TrackedItem],
    client: reqwest::Client,
    base_url: &str,
) -> Result<PollOutcome, PollFailure> {
    poll_workspace_with_options(app_id, encrypted_key, key, connections, tracked, client, base_url,
        PollOptions { cache: None, cache_scope: "", since: None }).await
}

/// The existing entry point performs a full, uncached reconciliation. Callers opting
/// into deltas must not treat them as successful full reconciliations; the outcome
/// carries an error to prevent the existing mode-handoff path from doing so.
pub(crate) async fn poll_workspace_with_options(
    app_id: i64,
    encrypted_key: &[u8],
    key: &[u8; 32],
    connections: &[Connection],
    tracked: &[TrackedItem],
    client: reqwest::Client,
    base_url: &str,
    options: PollOptions<'_>,
) -> Result<PollOutcome, PollFailure> {
    if options.cache.is_some() && options.cache_scope.is_empty() {
        return Err(PollFailure { message: "GitHub cache scope is required".into(), rate_limit: RateLimit::default() });
    }
    let jwt = app_jwt(app_id, encrypted_key, key).map_err(|message| PollFailure {
        message,
        rate_limit: RateLimit::default(),
    })?;
    let mut api = Api {
        client,
        base: reqwest::Url::parse(base_url).map_err(|_| PollFailure {
            message: "invalid GitHub API URL".into(),
            rate_limit: RateLimit::default(),
        })?,
        rate: RateLimit::default(),
    };
    let mut outcome = PollOutcome::default();
    let mut discovery_budget = DiscoveryBudget::default();
    let mut proposal_bytes = 0_usize;
    if options.since.is_some() {
        outcome.errors.push(RepositoryError {
            repository: "poll".into(),
            message: "incremental poll is not a complete reconciliation".into(),
        });
    }
    outcome.installations = api
        .pages("/app/installations?per_page=100", &jwt, &mut discovery_budget)
        .await
        .map_err(|message| PollFailure {
            message,
            rate_limit: api.rate.clone(),
        })?;
    let mut tokens = HashMap::new();
    for installation in &outcome.installations {
        let path = format!("/app/installations/{}/access_tokens", installation.id);
        let token: Result<(Token, _), _> = api
            .request(
                reqwest::Method::POST,
                api.url(&path).expect("static URL path"),
                &jwt,
            )
            .await;
        let token = match token {
            Ok((token, _)) if !token.token.is_empty() && token.token.len() <= MAX_INSTALLATION_TOKEN => token.token,
            Ok(_) => {
                outcome.errors.push(RepositoryError {
                    repository: format!("installation:{}", installation.id),
                    message: "invalid GitHub installation token".into(),
                });
                continue;
            }
            Err(message) => {
                outcome.errors.push(RepositoryError {
                    repository: format!("installation:{}", installation.id),
                    message,
                });
                continue;
            }
        };
        match api.repositories(&token, &mut discovery_budget).await {
            Ok(repos) => {
                outcome
                    .repositories
                    .extend(repos.into_iter().map(|repo| InstalledRepository {
                        installation_id: installation.id,
                        full_name: repo.full_name,
                    }))
            }
            Err(message) => {
                outcome.errors.push(RepositoryError {
                    repository: format!("installation:{}", installation.id),
                    message,
                });
                continue;
            }
        }
        tokens.insert(installation.id, token);
    }
    let mut groups: HashMap<(i64, String), Vec<&Connection>> = HashMap::new();
    for connection in connections {
        groups
            .entry((connection.installation_id, connection.repository.to_ascii_lowercase()))
            .or_default()
            .push(connection);
    }
    for ((installation_id, _), mappings) in groups {
        let repository = mappings[0].repository.as_str();
        if connections.iter().any(|connection| {
            connection.repository.eq_ignore_ascii_case(repository)
                && connection.installation_id != installation_id
        }) {
            outcome.errors.push(RepositoryError {
                repository: repository.into(),
                message: "repository configured under multiple installations".into(),
            });
            continue;
        }
        let Some(token) = tokens.get(&installation_id) else {
            outcome.errors.push(RepositoryError {
                repository: repository.into(),
                message: "installation unavailable".into(),
            });
            continue;
        };
        if !outcome.repositories.iter().any(|r| {
            r.installation_id == installation_id && r.full_name.eq_ignore_ascii_case(repository)
        }) {
            outcome.errors.push(RepositoryError {
                repository: repository.into(),
                message: "repository not accessible to installation".into(),
            });
            continue;
        }
        let Some((owner, name)) = repository.split_once('/') else {
            outcome.errors.push(RepositoryError {
                repository: repository.into(),
                message: "invalid repository".into(),
            });
            continue;
        };
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            outcome.errors.push(RepositoryError {
                repository: repository.into(),
                message: "invalid repository".into(),
            });
            continue;
        }
        let owner =
            percent_encoding::utf8_percent_encode(owner, percent_encoding::NON_ALPHANUMERIC);
        let name = percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC);
        let path = format!("/repos/{owner}/{name}/issues?state=all&per_page=100");
        let mut url = api.url(&path).expect("static URL path");
        if let Some(since) = options.since {
            url.query_pairs_mut().append_pair("since", since);
        }
        // Include app, installation and API origin in addition to the caller's
        // generation scope; the complete URL includes the query and page number.
        let scope = format!("{:?}:{app_id}:{installation_id}:{}", options.cache_scope, api.base.origin().ascii_serialization());
        let items: Vec<Item> = match api.issue_pages(url, token, &scope, options.cache, outcome.page_updates.len(), proposal_bytes).await {
            Ok((items, updates)) => {
                for update in &updates {
                    proposal_bytes += update.scope.len() + update.url.len() + update.page.as_ref().map_or(0, |page| {
                        page.etag.len() + page.body.len() + page.next.as_ref().map_or(0, String::len)
                    });
                }
                outcome.page_updates.extend(updates);
                items
            },
            Err(message) => {
                outcome.errors.push(RepositoryError {
                    repository: repository.into(),
                    message,
                });
                continue;
            }
        };
        let seen_labeled: HashSet<i64> = items.iter()
            .filter(|item| mappings.iter().any(|mapping| item.labels.iter().any(|label| label.name == mapping.label)))
            .map(|item| item.number)
            .collect();
        for item in items {
            outcome.items_seen += 1;
            let kind = if item.pull_request.is_some() {
                "pull_request"
            } else {
                "issue"
            };
            if item.number <= 0 || !matches!(item.state.as_str(), "open" | "closed") {
                outcome.errors.push(RepositoryError {
                    repository: repository.into(),
                    message: "invalid item state or number".into(),
                });
                continue;
            }
            let selected: Vec<_> = mappings
                .iter()
                .filter(|mapping| item.labels.iter().any(|label| label.name == mapping.label))
                .collect();
            if selected.is_empty() {
                continue;
            }
            if selected.len() != 1 {
                outcome.errors.push(RepositoryError {
                    repository: repository.into(),
                    message: format!("ambiguous labels on #{}", item.number),
                });
                continue;
            }
            let description = item.body.unwrap_or_default();
            let title = item.title.trim().to_owned();
            if title.is_empty()
                || title.chars().count() > 500
                || description.chars().count() > 100_000
            {
                outcome.errors.push(RepositoryError {
                    repository: repository.into(),
                    message: format!("invalid content on #{}", item.number),
                });
                continue;
            }
            let state = if kind == "pull_request" && item.state == "closed" {
                let path = format!("/repos/{owner}/{name}/pulls/{}", item.number);
                let pull: Result<(Pull, _), _> = api
                    .request(
                        reqwest::Method::GET,
                        api.url(&path).expect("static URL path"),
                        token,
                    )
                    .await;
                match pull {
                    Ok((pull, _)) if pull.merged_at.is_some() => "merged",
                    Ok(_) => "closed",
                    Err(message) => {
                        outcome.errors.push(RepositoryError {
                            repository: repository.into(),
                            message: format!("#{}: {message}", item.number),
                        });
                        continue;
                    }
                }
            } else if item.state == "closed" {
                "closed"
            } else {
                "open"
            };
            let observation = GithubWorkItem {
                repository: repository.into(),
                number: item.number,
                project_id: selected[0].project_id,
                title,
                description,
                kind,
                state,
                state_changed: false,
            };
            outcome.actions.push(PollAction::Observe(observation));
        }
        // This runs only after every page succeeded. A missing item is not evidence by
        // itself: confirm it directly and reject redirects, moves and ambiguous labels.
        for link in tracked.iter().filter(|link| {
            options.since.is_none() &&
            link.repository.eq_ignore_ascii_case(repository) && !seen_labeled.contains(&link.number)
        }) {
            if link.number <= 0 || !matches!(link.kind.as_str(), "issue" | "pull_request") {
                outcome.errors.push(RepositoryError { repository: repository.into(), message: format!("invalid tracked item #{}", link.number) });
                continue;
            }
            let path = format!("/repos/{owner}/{name}/issues/{}", link.number);
            let confirmed: Result<(Item, _), _> = api.request(reqwest::Method::GET, api.url(&path).expect("static URL path"), token).await;
            match confirmed {
                Ok((fresh, _)) => {
                    let repo_url = api.url(&format!("/repos/{owner}/{name}")).expect("static URL path");
                    if confirmed_removed(&fresh, link, &mappings, &repo_url) {
                        outcome.actions.push(PollAction::Pause {
                            repository: link.repository.clone(), number: link.number,
                            kind: if link.kind == "issue" { "issue" } else { "pull_request" },
                        });
                    } else {
                        // A changed or inconsistent observation is not evidence of removal,
                        // nor is it a complete successful poll suitable for a mode handoff.
                        outcome.errors.push(RepositoryError {
                            repository: repository.into(),
                            message: format!("inconclusive confirmation for #{}", link.number),
                        });
                    }
                }
                Err(message) => outcome.errors.push(RepositoryError {
                    repository: repository.into(), message: format!("confirmation for #{}: {message}", link.number),
                }),
            }
        }
    }
    outcome.rate_limit = api.rate;
    Ok(outcome)
}

fn confirmed_removed(item: &Item, link: &TrackedItem, mappings: &[&Connection], repository_url: &reqwest::Url) -> bool {
    let Some(actual_repo) = item.repository_url.as_deref().and_then(|url| reqwest::Url::parse(url).ok()) else {
        return false;
    };
    item.number == link.number
        && (item.pull_request.is_some() == (link.kind == "pull_request"))
        && matches!(item.state.as_str(), "open" | "closed")
        && actual_repo.origin() == repository_url.origin()
        && actual_repo.path().eq_ignore_ascii_case(repository_url.path())
        && actual_repo.query().is_none() && actual_repo.fragment().is_none()
        && !mappings.iter().any(|mapping| item.labels.iter().any(|label| label.name == mapping.label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_budget_is_shared_and_rejects_overflow() {
        let mut budget = DiscoveryBudget::default();
        for _ in 0..MAX_DISCOVERY_PAGES {
            budget.start_page().unwrap();
        }
        assert!(budget.start_page().is_err());
        budget.add_body(MAX_DISCOVERY_BODY).unwrap();
        assert!(budget.add_body(1).is_err());
        assert!(budget.add_body(usize::MAX).is_err());
    }

    fn item(json: &str) -> Item {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn confirmation_requires_matching_identity_repository_and_state() {
        let link = TrackedItem { repository: "owner/repo".into(), number: 42, kind: "issue".into() };
        let repo = reqwest::Url::parse("https://api.github.com/repos/owner/repo").unwrap();
        let make = |number: i64, state: &str, repository: &str, pull: &str| item(&format!(
            r#"{{"number":{number},"title":"x","body":null,"state":"{state}","labels":[],"pull_request":{pull},"repository_url":"{repository}"}}"#
        ));
        let url = "https://api.github.com/repos/owner/repo";
        assert!(confirmed_removed(&make(42, "open", url, "null"), &link, &[], &repo));
        assert!(!confirmed_removed(&make(43, "open", url, "null"), &link, &[], &repo));
        assert!(!confirmed_removed(&make(42, "unknown", url, "null"), &link, &[], &repo));
        assert!(!confirmed_removed(&make(42, "open", "https://api.github.com/repos/owner/other", "null"), &link, &[], &repo));
        assert!(!confirmed_removed(&make(42, "open", url, "{}"), &link, &[], &repo));
        assert!(!confirmed_removed(&item(r#"{"number":42,"title":"x","body":null,"state":"open","labels":[]}"#), &link, &[], &repo));
    }

    #[test]
    fn confirmation_with_a_configured_label_never_pauses() {
        // A matching label may mean the item moved to another configured project.
        let link = TrackedItem { repository: "owner/repo".into(), number: 42, kind: "issue".into() };
        let repo = reqwest::Url::parse("https://api.github.com/repos/owner/repo").unwrap();
        let connection = Connection { installation_id: 1, repository: "owner/repo".into(), label: "orbit".into(), project_id: Id::new_v7() };
        let fresh = item(r#"{"number":42,"title":"x","body":null,"state":"closed","labels":[{"name":"orbit"}],"repository_url":"https://api.github.com/repos/owner/repo"}"#);
        assert!(!confirmed_removed(&fresh, &link, &[&connection], &repo));
    }
}
