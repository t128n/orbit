//! Executes a claimed workspace poll. The scheduler owns claiming and calls `run` once per claim.

use std::{collections::HashMap, sync::Arc, time::Duration};

use futures_util::{StreamExt, stream};
use orbit_platform::{Database, Id, Row, TimestampMillis};

use super::{CachedPage, Connection, PageCache, PollAction, PollOptions, PollOutcome, RateLimit, load_tracked_items, poll_workspace_with_options};
use crate::{
    github_poll_schedule::{self as schedule, Claim, Outcome},
    github_poll_transition,
    integration_routes::{IntegrationState, pause_github_work_item_fenced},
    repositories::{
        api_tokens::ApiTokenRepository,
        tasks::{PollFence, TaskRepository},
    },
};

/// Dispatch due work from a durable recurring job. Claims are independently fenced,
/// so retries, multiple servers and overlapping jobs cannot apply the same generation.
pub(crate) async fn dispatch(
    database: Database,
    key: Option<[u8; 32]>,
    github_host: orbit_platform::GitHubHost,
) -> Result<(), sqlx::Error> {
    const MAX_CONCURRENT_WORKSPACES: usize = 4;
    const LEASE_MS: i64 = 120_000;

    let workspaces: Vec<String> = sqlx::query_scalar(
        "SELECT c.workspace_id FROM github_workspace_settings c LEFT JOIN github_poll_state s ON s.workspace_id = c.workspace_id WHERE c.sync_mode = 'polling' ORDER BY CASE WHEN s.last_started_at IS NULL THEN 0 ELSE 1 END, s.last_started_at, c.workspace_id",
    ).fetch_all(database.pool()).await?;
    let rest_base = Arc::new(github_host.rest_base());
    let tasks = Arc::new(TaskRepository::new(database.clone()).with_github_host(github_host));
    // The futures are owned by the dispatch job, not detached tasks. Dropping the job
    // cancels them, stops lease renewal, and lets unfinished claims expire safely.
    let mut workers = stream::iter(workspaces.into_iter().map(|workspace| {
        let database = database.clone();
        let tasks = tasks.clone();
        let rest_base = Arc::clone(&rest_base);
        async move {
            let owner = Id::new_v7().to_string();
            let Some(claim) = schedule::try_claim(&database, &workspace, &owner, LEASE_MS).await?
            else {
                return Ok(());
            };
            if let Err(error) = run(
                tasks,
                &claim,
                key.as_ref(),
                &rest_base,
                LEASE_MS,
            )
            .await
            {
                tracing::warn!(workspace_id = %workspace, error = ?error, "GitHub polling worker failed");
                // Do not release a claim after a database error: the lease provides safe recovery.
            }
            Ok(())
        }
    }))
    .buffer_unordered(MAX_CONCURRENT_WORKSPACES);
    let mut claim_error = None;
    while let Some(result) = workers.next().await {
        if let Err(error) = result {
            tracing::warn!(error = ?error, "GitHub polling claim failed");
            if claim_error.is_none() {
                claim_error = Some(error);
            }
        }
    }
    match claim_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Result of running a claim. `Lost` means no further effects or completion were committed
/// by this worker after the claim became invalid.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RunResult {
    Finished,
    Lost,
}

/// Run with a fresh client that cannot follow redirects. Only trusted test code should
/// override `base_url`; production callers should pass `https://api.github.com/`.
/// A lease is renewed while HTTP and database work are in progress. All persistence is
/// fenced independently, so cancellation or a failed renewal cannot admit stale writes.
pub(crate) async fn run(
    tasks: Arc<TaskRepository>,
    claim: &Claim,
    encryption_key: Option<&[u8; 32]>,
    base_url: &str,
    lease_ms: i64,
) -> Result<RunResult, sqlx::Error> {
    if lease_ms < 3 {
        return Err(sqlx::Error::Protocol("invalid GitHub polling lease".into()));
    }
    let database = tasks.database().clone();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|_| sqlx::Error::Protocol("cannot build GitHub client".into()))?;
    let operation = execute(&database, tasks, claim, encryption_key, client, base_url);
    tokio::pin!(operation);
    let mut interval = tokio::time::interval(Duration::from_millis((lease_ms / 3) as u64));
    interval.tick().await;
    loop {
        tokio::select! {
            result = &mut operation => return result,
            _ = interval.tick() => {
                if !schedule::renew(&database, claim, lease_ms).await? {
                    return Ok(RunResult::Lost);
                }
            }
        }
    }
}

async fn execute(
    database: &Database,
    tasks: Arc<TaskRepository>,
    claim: &Claim,
    key: Option<&[u8; 32]>,
    client: reqwest::Client,
    base_url: &str,
) -> Result<RunResult, sqlx::Error> {
    let workspace: Id = claim
        .workspace_id
        .parse()
        .map_err(|_| sqlx::Error::Protocol("invalid polling workspace".into()))?;
    let loaded = load(database, claim).await;
    let (app_id, encrypted_key, connections, actor) = match loaded {
        Ok(Some(value)) => value,
        Ok(None) => return Ok(RunResult::Lost),
        Err(error) => {
            tracing::warn!(error = ?error, "GitHub poll configuration failed");
            return finish(
                database,
                claim,
                false,
                "configuration failed",
                0,
                0,
                &RateLimit::default(),
            )
            .await;
        }
    };
    let Some(key) = key else {
        return finish(
            database,
            claim,
            false,
            "GitHub encryption key unavailable",
            0,
            0,
            &RateLimit::default(),
        )
        .await;
    };
    let tracked = match load_tracked_items(database, &claim.workspace_id).await {
        Ok(items) => items,
        Err(error) => {
            tracing::warn!(error = ?error, "GitHub poll tracked items failed");
            return finish(
                database,
                claim,
                false,
                "loading tracked items failed",
                0,
                0,
                &RateLimit::default(),
            )
            .await;
        }
    };
    // Include the credential ciphertext in the scope even if a credential writer fails
    // to bump the configuration version. It is hashed, never stored as a credential.
    use sha2::{Digest, Sha256};
    let credential_hash = format!("{:x}", Sha256::digest(&encrypted_key));
    let cache_scope = format!("{}:{}:{}:{}", claim.workspace_id, claim.config_version, claim.mode_epoch, credential_hash);
    let scope_prefix = format!("{:?}:{app_id}:", cache_scope);
    let mut cache = load_pages(database, claim, key, &scope_prefix).await.unwrap_or_default();
    // Consume the persisted snapshot before using it. If this scan is cancelled or
    // loses its lease, a successor must not inherit representations whose replacement
    // proposals were never committed. The fence prevents deleting a successor's cache.
    if !consume_pages(database, claim).await? {
        return Ok(RunResult::Lost);
    }
    let mut outcome = match poll_workspace_with_options(
        app_id,
        &encrypted_key,
        key,
        &connections,
        &tracked,
        client,
        base_url,
        PollOptions { cache: Some(&cache), cache_scope: &cache_scope, since: None },
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            return finish(
                database,
                claim,
                false,
                &error.message,
                0,
                0,
                &error.rate_limit,
            )
            .await;
        }
    };
    let seen = i64::try_from(outcome.items_seen).unwrap_or(i64::MAX);
    let mut failed = !outcome.errors.is_empty();
    for error in &outcome.errors {
        tracing::warn!(repository = %error.repository, error = %error.message, "GitHub poll incomplete");
    }
    match persist_discovery(database, claim, &outcome).await {
        Ok(true) => {}
        Ok(false) => return Ok(RunResult::Lost),
        Err(error) => {
            tracing::warn!(error = ?error, "GitHub poll discovery failed");
            failed = true;
        }
    }
    let state = IntegrationState::new(
        Arc::new(ApiTokenRepository::new(database.clone())),
        tasks.clone(),
    );
    let fence = PollFence::from(claim);
    let mut updated = 0_i64;
    for action in std::mem::take(&mut outcome.actions) {
        match action {
            PollAction::Observe(item) => match tasks
                .observe_github_work_item(
                    workspace,
                    actor,
                    item,
                    "github-poll",
                    TimestampMillis::now(),
                    &fence,
                )
                .await
            {
                Ok(Some(_)) => updated = updated.saturating_add(1),
                Ok(None) => return Ok(RunResult::Lost),
                Err(error) => {
                    tracing::warn!(error = ?error, "GitHub poll observation failed");
                    failed = true;
                }
            },
            PollAction::Pause {
                repository,
                number,
                kind,
            } => match pause_github_work_item_fenced(
                &state,
                &claim.workspace_id,
                &repository,
                number,
                kind,
                None,
                &fence,
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => return Ok(RunResult::Lost),
                Err(_) => {
                    tracing::warn!("GitHub poll pause failed");
                    failed = true;
                }
            },
        }
    }
    // An incomplete poll may have seen fresh pages whose updates were discarded.
    // Clear the old cache rather than reuse potentially stale representations later.
    if failed {
        cache = Pages::default();
    }
    {
        match persist_pages(database, claim, key, app_id, &encrypted_key, &scope_prefix, &mut cache, &outcome, !failed).await {
            Ok(true) => {},
            Ok(false) => return Ok(RunResult::Lost),
            Err(error) => {
                tracing::warn!(error = ?error, "GitHub poll cache persistence failed");
                failed = true;
            }
        }
    }
    if !failed && github_poll_transition::complete_applied(database, claim, &outcome).await? {
        // A webhooks handoff invalidates and releases this claim. Polling handoffs
        // retain it, so record their result normally.
        let mut tx = database.immediate_transaction().await?;
        let current = schedule::is_current(&mut tx, database.backend(), claim).await?;
        tx.commit().await?;
        if !current {
            return Ok(RunResult::Finished);
        }
    }
    finish(
        database,
        claim,
        !failed,
        if failed {
            "poll incomplete"
        } else {
            "poll complete"
        },
        seen,
        updated,
        &outcome.rate_limit,
    )
    .await
}

// The body is encoded as a JSON array of numbers, so its serialized size can be
// several times larger than the response. Bound the actual encoding as well.
const MAX_CACHE_JSON: usize = 4 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 512;
const MAX_PAGE_BODY: usize = 256 * 1024;
const MAX_CACHE_FIELD: usize = 2048;
const MAX_CACHE_CIPHERTEXT: i64 = MAX_CACHE_JSON as i64 + 28;

#[derive(Default)]
struct Pages {
    entries: HashMap<(String, String), CachedPage>,
    encoded_size: usize,
}

fn entry_size(scope: &str, url: &str, page: &CachedPage) -> Option<usize> {
    if scope.len() > MAX_CACHE_FIELD || url.len() > MAX_CACHE_FIELD
        || page.etag.is_empty() || page.etag.len() > MAX_CACHE_FIELD
        || page.body.len() > MAX_PAGE_BODY || !safe_cache_url(url)
        || page.next.as_deref().is_some_and(|next| next.len() > MAX_CACHE_FIELD || !safe_cache_url(next))
    {
        return None;
    }
    // Account for a comma per entry, plus the enclosing array separately.
    serde_json::to_vec(&(scope, url, &page.etag, &page.body, &page.next)).ok().map(|v| v.len() + 1)
}

impl PageCache for Pages {
    fn get(&self, scope: &str, url: &str) -> Option<CachedPage> {
        self.entries.get(&(scope.to_owned(), url.to_owned())).cloned()
    }

    fn put(&mut self, scope: &str, url: &str, page: CachedPage) {
        let key = (scope.to_owned(), url.to_owned());
        // An uncacheable replacement must not leave an older representation behind.
        if let Some(old) = self.entries.remove(&key) {
            self.encoded_size -= entry_size(scope, url, &old).unwrap_or(0);
        }
        let Some(size) = entry_size(scope, url, &page) else { return; };
        if self.entries.len() >= MAX_CACHE_ENTRIES || self.encoded_size + size + 2 > MAX_CACHE_JSON {
            return;
        }
        self.encoded_size += size;
        self.entries.insert(key, page);
    }
}

impl Pages {
    fn remove(&mut self, scope: &str, url: &str) {
        if let Some(old) = self.entries.remove(&(scope.to_owned(), url.to_owned())) {
            self.encoded_size -= entry_size(scope, url, &old).unwrap_or(0);
        }
    }
}

async fn load_pages(database: &Database, claim: &Claim, key: &[u8; 32], scope_prefix: &str) -> Result<Pages, sqlx::Error> {
    let encrypted: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT pages_encrypted FROM github_poll_page_cache WHERE workspace_id = $1 AND config_version = $2 AND length(pages_encrypted) <= $3",
    ).bind(&claim.workspace_id).bind(claim.config_version).bind(MAX_CACHE_CIPHERTEXT).fetch_optional(database.pool()).await?;
    let Some(encrypted) = encrypted else { return Ok(Pages::default()); };
    let Ok(json) = crate::secret_box::decrypt_secret(key, &encrypted) else { return Ok(Pages::default()); };
    let Ok(entries) = serde_json::from_str::<Vec<(String, String, String, Vec<u8>, Option<String>)>>(&json) else {
        return Ok(Pages::default());
    };
    let mut pages = Pages::default();
    for (scope, url, etag, body, next) in entries {
        if scope.starts_with(scope_prefix) {
            pages.put(&scope, &url, CachedPage { etag, body, next });
        }
    }
    Ok(pages)
}

async fn consume_pages(database: &Database, claim: &Claim) -> Result<bool, sqlx::Error> {
    let mut tx = database.immediate_transaction().await?;
    if !schedule::is_current(&mut tx, database.backend(), claim).await? {
        return Ok(false);
    }
    sqlx::query("DELETE FROM github_poll_page_cache WHERE workspace_id = $1")
        .bind(&claim.workspace_id).execute(&mut *tx).await?;
    if !schedule::is_current(&mut tx, database.backend(), claim).await? {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

async fn persist_pages(database: &Database, claim: &Claim, key: &[u8; 32], app_id: i64, encrypted_key: &[u8], scope_prefix: &str, cache: &mut Pages, outcome: &PollOutcome, apply_updates: bool) -> Result<bool, sqlx::Error> {
    // Keep representations used by 304 responses; proposals only contain new pages.
    for update in outcome.page_updates.iter().filter(|_| apply_updates) {
        if update.scope.starts_with(scope_prefix) {
            match &update.page {
                Some(page) => cache.put(&update.scope, &update.url, page.clone()),
                None => cache.remove(&update.scope, &update.url),
            }
        }
    }
    let entries: Vec<_> = cache.entries.iter().map(|((scope, url), page)| (
        scope, url, &page.etag, &page.body, &page.next,
    )).collect();
    let json = serde_json::to_string(&entries).map_err(|_| sqlx::Error::Protocol("cannot encode GitHub page cache".into()))?;
    let encrypted = crate::secret_box::encrypt_secret(key, &json).map_err(|_| sqlx::Error::Protocol("cannot encrypt GitHub page cache".into()))?;
    let mut tx = database.immediate_transaction().await?;
    if !schedule::is_current(&mut tx, database.backend(), claim).await? {
        return Ok(false);
    }
    let current_app = sqlx::query("SELECT app_id, private_key_encrypted FROM github_apps WHERE workspace_id = $1")
        .bind(&claim.workspace_id).fetch_optional(&mut *tx).await?;
    if !current_app.is_some_and(|row| row.get::<i64, _>("app_id") == app_id
        && row.get::<Vec<u8>, _>("private_key_encrypted").as_slice() == encrypted_key) {
        return Ok(false);
    }
    let workspace = claim.workspace_id.parse().map_err(|_| sqlx::Error::Protocol("invalid polling workspace".into()))?;
    if crate::repositories::billing::write_access_in(database, &mut tx, workspace).await?
        == crate::billing::WorkspaceAccess::ReadOnly
    {
        return Ok(false);
    }
    sqlx::query("INSERT INTO github_poll_page_cache (workspace_id, config_version, pages_encrypted) VALUES ($1, $2, $3) ON CONFLICT (workspace_id) DO UPDATE SET config_version = excluded.config_version, pages_encrypted = excluded.pages_encrypted")
        .bind(&claim.workspace_id).bind(claim.config_version).bind(encrypted).execute(&mut *tx).await?;
    if !schedule::is_current(&mut tx, database.backend(), claim).await? {
        return Ok(false);
    }
    if crate::repositories::billing::write_access_in(database, &mut tx, workspace).await?
        == crate::billing::WorkspaceAccess::ReadOnly
    {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

// Do not persist userinfo or arbitrary query parameters from an untrusted Link header.
// A URL outside the poller's known full-listing shape simply is not cached.
fn safe_cache_url(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else { return false; };
    url.username().is_empty() && url.password().is_none() && url.fragment().is_none()
        && url.query_pairs().all(|(key, value)| match key.as_ref() {
            "state" => value == "all",
            "page" | "per_page" => !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
            _ => false,
        })
}

async fn load(
    database: &Database,
    claim: &Claim,
) -> Result<Option<(i64, Vec<u8>, Vec<Connection>, Id)>, sqlx::Error> {
    let mut tx = database.immediate_transaction().await?;
    if !schedule::is_current(&mut tx, database.backend(), claim).await? {
        return Ok(None);
    }
    let app = sqlx::query(
        "SELECT app_id, private_key_encrypted FROM github_apps WHERE workspace_id = $1",
    )
    .bind(&claim.workspace_id)
    .fetch_one(&mut *tx)
    .await?;
    let rows = sqlx::query("SELECT c.installation_id, c.repository, c.label, c.project_id FROM github_project_connections c JOIN github_installations i ON i.installation_id = c.installation_id AND i.workspace_id = c.workspace_id JOIN projects p ON p.id = c.project_id AND p.workspace_id = c.workspace_id WHERE c.workspace_id = $1")
        .bind(&claim.workspace_id).fetch_all(&mut *tx).await?;
    let mut connections = Vec::with_capacity(rows.len());
    for row in rows {
        let project: String = row.try_get("project_id")?;
        connections.push(Connection {
            installation_id: row.try_get("installation_id")?,
            repository: row.try_get("repository")?,
            label: row.try_get("label")?,
            project_id: project
                .parse()
                .map_err(|_| sqlx::Error::Protocol("invalid GitHub project".into()))?,
        });
    }
    let actor: String = sqlx::query_scalar("SELECT user_id FROM memberships WHERE workspace_id = $1 AND role = 'owner' ORDER BY user_id LIMIT 1")
        .bind(&claim.workspace_id).fetch_one(&mut *tx).await?;
    let actor = actor
        .parse()
        .map_err(|_| sqlx::Error::Protocol("invalid GitHub actor".into()))?;
    let result = (
        app.try_get("app_id")?,
        app.try_get("private_key_encrypted")?,
        connections,
        actor,
    );
    tx.commit().await?;
    Ok(Some(result))
}

/// Reconcile complete discovery without cascading away configured project connections.
async fn persist_discovery(
    database: &Database,
    claim: &Claim,
    outcome: &PollOutcome,
) -> Result<bool, sqlx::Error> {
    let mut tx = database.immediate_transaction().await?;
    if !schedule::is_current(&mut tx, database.backend(), claim).await? {
        return Ok(false);
    }
    let workspace = claim
        .workspace_id
        .parse()
        .map_err(|_| sqlx::Error::Protocol("invalid polling workspace".into()))?;
    if crate::repositories::billing::write_access_in(database, &mut tx, workspace).await?
        == crate::billing::WorkspaceAccess::ReadOnly
    {
        return Ok(false);
    }
    for installation in &outcome.installations {
        if installation.id <= 0 || installation.account.login.is_empty() {
            return Err(sqlx::Error::Protocol("invalid GitHub installation".into()));
        }
        sqlx::query("INSERT INTO github_installations (installation_id, workspace_id, account_login) VALUES ($1, $2, $3) ON CONFLICT (installation_id) DO UPDATE SET account_login = excluded.account_login WHERE github_installations.workspace_id = excluded.workspace_id")
            .bind(installation.id).bind(&claim.workspace_id).bind(&installation.account.login).execute(&mut *tx).await?;
        let owner: Option<String> = sqlx::query_scalar(
            "SELECT workspace_id FROM github_installations WHERE installation_id = $1",
        )
        .bind(installation.id)
        .fetch_optional(&mut *tx)
        .await?;
        if owner.as_deref() != Some(claim.workspace_id.as_str()) {
            return Err(sqlx::Error::Protocol(
                "GitHub installation belongs to another workspace".into(),
            ));
        }
    }
    for repository in &outcome.repositories {
        // Never write under an installation owned by another workspace, even if a response
        // somehow contains an installation not present in the current discovery.
        if !outcome
            .installations
            .iter()
            .any(|i| i.id == repository.installation_id)
        {
            return Err(sqlx::Error::Protocol("unknown GitHub installation".into()));
        }
        sqlx::query("INSERT INTO github_installation_repositories (installation_id, repository) SELECT installation_id, $1 FROM github_installations WHERE installation_id = $2 AND workspace_id = $3 ON CONFLICT (installation_id, repository) DO NOTHING")
            .bind(&repository.full_name).bind(repository.installation_id).bind(&claim.workspace_id).execute(&mut *tx).await?;
    }
    // These two errors are produced locally when a configured connection is absent
    // from otherwise successful discovery. All other errors conservatively prevent
    // removals, including token, pagination, and incremental-poll failures.
    let complete = outcome.errors.iter().all(|error| {
        error.message == "installation unavailable"
            || error.message == "repository not accessible to installation"
    }) && !outcome.errors.iter().any(|error| error.repository.starts_with("installation:"));
    if complete {
        let rows = sqlx::query("SELECT r.installation_id, r.repository FROM github_installation_repositories r JOIN github_installations i ON i.installation_id = r.installation_id WHERE i.workspace_id = $1")
            .bind(&claim.workspace_id).fetch_all(&mut *tx).await?;
        for row in rows {
            let installation_id: i64 = row.try_get("installation_id")?;
            let repository: String = row.try_get("repository")?;
            if !outcome.repositories.iter().any(|found| {
                found.installation_id == installation_id
                    && found.full_name.eq_ignore_ascii_case(&repository)
            }) {
                // Repository listings represent accessibility; connections do not
                // reference them and remain intact if access is later restored.
                sqlx::query("DELETE FROM github_installation_repositories WHERE installation_id = $1 AND repository = $2")
                    .bind(installation_id).bind(&repository).execute(&mut *tx).await?;
            }
        }
        let rows = sqlx::query("SELECT installation_id FROM github_installations WHERE workspace_id = $1")
            .bind(&claim.workspace_id).fetch_all(&mut *tx).await?;
        for row in rows {
            let installation_id: i64 = row.try_get("installation_id")?;
            if !outcome.installations.iter().any(|found| found.id == installation_id) {
                // There is no installation status column. Retain a missing parent
                // when connected: deleting it would cascade-delete project mappings.
                // Its empty repository listing is the only accessibility marker.
                sqlx::query("DELETE FROM github_installations WHERE workspace_id = $1 AND installation_id = $2 AND NOT EXISTS (SELECT 1 FROM github_project_connections WHERE installation_id = $2)")
                    .bind(&claim.workspace_id).bind(installation_id).execute(&mut *tx).await?;
            }
        }
    }
    if !schedule::is_current(&mut tx, database.backend(), claim).await? {
        return Ok(false);
    }
    if crate::repositories::billing::write_access_in(database, &mut tx, workspace).await?
        == crate::billing::WorkspaceAccess::ReadOnly
    {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

async fn finish(
    database: &Database,
    claim: &Claim,
    success: bool,
    message: &str,
    seen: i64,
    updated: i64,
    rate: &RateLimit,
) -> Result<RunResult, sqlx::Error> {
    let outcome = Outcome {
        success,
        status: if success { "success" } else { "failure" },
        result: Some(message),
        items_seen: seen,
        items_updated: updated,
        rate_limit: schedule::RateLimit {
            remaining: rate.remaining,
            reset_epoch_seconds: rate.reset.and_then(|v| i64::try_from(v).ok()),
            retry_after_seconds: rate.retry_after,
        },
    };
    Ok(if schedule::finish(database, claim, &outcome).await? {
        RunResult::Finished
    } else {
        RunResult::Lost
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repositories::identity::{IdentityRepository, SetupRequest};
    use orbit_platform::{PasswordService, TestDatabase};

    #[test]
    fn removing_or_replacing_uncacheable_page_discards_old_representation() {
        let mut pages = Pages::default();
        let url = "https://api.github.com/repos/a/b/issues?state=all&per_page=100";
        let page = CachedPage { etag: "\"old\"".into(), body: b"[]".to_vec(), next: None };
        pages.put("scope", url, page.clone());
        assert!(pages.get("scope", url).is_some());
        pages.remove("scope", url);
        assert!(pages.get("scope", url).is_none());
        assert_eq!(pages.encoded_size, 0);
        pages.put("scope", url, page);
        pages.put("scope", url, CachedPage { etag: String::new(), body: b"[]".to_vec(), next: None });
        assert!(pages.get("scope", url).is_none());
        assert_eq!(pages.encoded_size, 0);
    }

    #[tokio::test]
    async fn missing_configuration_records_failure_and_stale_claim_cannot_finish() {
        let database = TestDatabase::new().await.unwrap();
        let identity = IdentityRepository::new((*database).clone());
        let now = TimestampMillis::now();
        identity
            .store_setup_token(
                "setup",
                TimestampMillis::from_millis(now.as_millis() + 60_000),
            )
            .await
            .unwrap();
        let setup = identity
            .complete_setup(
                SetupRequest {
                    token: "setup".into(),
                    email: "owner@example.com".into(),
                    display_name: "Owner".into(),
                    password_hash: PasswordService::default()
                        .hash("correct horse battery")
                        .unwrap(),
                    workspace_name: "Orbit".into(),
                    project_name: "General".into(),
                },
                now,
            )
            .await
            .unwrap();
        let workspace = setup.workspace_id.to_string();
        sqlx::query("INSERT INTO github_workspace_settings (workspace_id, sync_mode) VALUES ($1, 'polling')")
            .bind(&workspace).execute(database.pool()).await.unwrap();
        let claim = schedule::try_claim(&database, &workspace, "worker", 120_000)
            .await
            .unwrap()
            .unwrap();
        crate::repositories::billing::enable(
            &database,
            orbit_platform::TrialConfig {
                days: 0,
                ..Default::default()
            },
        );
        sqlx::query("UPDATE workspaces SET billing_exempt = 0 WHERE id = $1")
            .bind(&workspace)
            .execute(database.pool())
            .await
            .unwrap();
        assert!(
            !persist_discovery(&database, &claim, &PollOutcome::default())
                .await
                .unwrap()
        );
        let mut tx = database.immediate_transaction().await.unwrap();
        assert!(
            !crate::repositories::tasks::poll_fence_current(
                &mut tx,
                &database,
                &workspace,
                &PollFence::from(&claim),
            )
            .await
            .unwrap()
        );
        tx.rollback().await.unwrap();
        sqlx::query("UPDATE workspaces SET billing_exempt = 1 WHERE id = $1")
            .bind(&workspace)
            .execute(database.pool())
            .await
            .unwrap();
        let tasks = Arc::new(TaskRepository::new((*database).clone()));
        assert_eq!(
            run(
                tasks,
                &claim,
                Some(&[1; 32]),
                "https://api.github.com/",
                120_000
            )
            .await
            .unwrap(),
            RunResult::Finished
        );
        let status: String =
            sqlx::query_scalar("SELECT last_status FROM github_poll_state WHERE workspace_id = $1")
                .bind(&workspace)
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(status, "failure");
        assert_eq!(
            finish(
                &database,
                &claim,
                true,
                "stale",
                0,
                0,
                &RateLimit::default()
            )
            .await
            .unwrap(),
            RunResult::Lost
        );
        assert!(
            !persist_discovery(&database, &claim, &PollOutcome::default())
                .await
                .unwrap()
        );
    }
}
