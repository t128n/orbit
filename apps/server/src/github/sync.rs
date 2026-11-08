//! Durable GitHub issue sync jobs: create/update issues, apply a pulled issue, and import a
//! repository's existing issues.
//!
//! Payloads carry ids and flags only. Titles, bodies, tokens and private keys never enter a job
//! payload, an audit row, or a log line.

use std::time::Duration;

use orbit_platform::{
    Database, Flag, Id, Job, JobError, JobKind, JobKindRegistrationError, JobStore, Row,
    TimestampMillis, Worker,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Any, Transaction};

use crate::audit::{self, AuditOutcome};
use crate::billing::WorkspaceAccess;
use crate::github::client::{AppCredentials, GithubClient, GithubError};
use crate::repositories::billing;
use crate::repositories::tasks::{GithubWorkItem, TaskError, TaskRepository};

pub const PUSH_JOB: &str = "github.issue.push";
pub const PULL_JOB: &str = "github.issue.pull";
pub const IMPORT_JOB: &str = "github.issue.import";

/// Why a push was enqueued. Only diagnostics and the audit trail use it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushReason {
    Created,
    Updated,
    Manual,
    Resolve,
}

impl PushReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Manual => "manual",
            Self::Resolve => "resolve",
        }
    }
}

fn payload_fields() -> [&'static str; 7] {
    [
        "workspace_id",
        "task_id",
        "actor_id",
        "request_id",
        "reason",
        "force",
        "import_id",
    ]
}

fn github_job_kind(name: &str) -> JobKind {
    JobKind::new(name)
        .with_lease(Duration::from_secs(5 * 60))
        .with_sensitive_fields(payload_fields())
}

#[must_use]
pub fn push_job_kind() -> JobKind {
    github_job_kind(PUSH_JOB)
}

#[must_use]
pub fn pull_job_kind() -> JobKind {
    github_job_kind(PULL_JOB)
}

#[must_use]
pub fn import_job_kind() -> JobKind {
    github_job_kind(IMPORT_JOB)
}

/// Enqueues a push when the task's project has a GitHub connection. The job is delayed a couple
/// of seconds so a burst of edits coalesces into one meaningful write.
pub async fn enqueue_push(
    transaction: &mut Transaction<'_, Any>,
    workspace_id: Id,
    task_id: Id,
    actor_id: Id,
    request_id: &str,
    reason: PushReason,
    force: bool,
    now: TimestampMillis,
) -> Result<bool, sqlx::Error> {
    if !project_is_connected(transaction, workspace_id, task_id).await? {
        return Ok(false);
    }
    let job = Job::new(
        push_job_kind(),
        json!({
            "workspace_id": workspace_id.to_string(),
            "task_id": task_id.to_string(),
            "actor_id": actor_id.to_string(),
            "request_id": bounded_request_id(request_id),
            "reason": reason.as_str(),
            "force": force,
        }),
        TimestampMillis::from_millis(now.as_millis().saturating_add(2_000)),
    )
    .with_workspace(workspace_id.to_string());
    JobStore::enqueue_in(transaction, &job)
        .await
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    Ok(true)
}

/// Enqueues a pull after an explicit "Use GitHub version" resolution.
pub async fn enqueue_pull(
    transaction: &mut Transaction<'_, Any>,
    workspace_id: Id,
    task_id: Id,
    actor_id: Id,
    request_id: &str,
    now: TimestampMillis,
) -> Result<bool, sqlx::Error> {
    if !project_is_connected(transaction, workspace_id, task_id).await? {
        return Ok(false);
    }
    let job = Job::new(
        pull_job_kind(),
        json!({
            "workspace_id": workspace_id.to_string(),
            "task_id": task_id.to_string(),
            "actor_id": actor_id.to_string(),
            "request_id": bounded_request_id(request_id),
        }),
        now,
    )
    .with_workspace(workspace_id.to_string());
    JobStore::enqueue_in(transaction, &job)
        .await
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    Ok(true)
}

/// Enqueues an import that the route created with a `github_issue_imports` row.
pub async fn enqueue_import(
    transaction: &mut Transaction<'_, Any>,
    workspace_id: Id,
    import_id: Id,
    now: TimestampMillis,
) -> Result<bool, sqlx::Error> {
    let Flag(connected) = sqlx::query_scalar::<_, Flag>(
        "SELECT EXISTS(SELECT 1 FROM github_issue_imports i \
         JOIN github_project_connections c ON c.project_id = i.project_id \
         AND c.workspace_id = i.workspace_id \
         WHERE i.id = $1 AND i.workspace_id = $2)",
    )
    .bind(import_id.to_string())
    .bind(workspace_id.to_string())
    .fetch_one(&mut **transaction)
    .await?;
    if !connected {
        return Ok(false);
    }
    let job = Job::new(
        import_job_kind(),
        json!({
            "workspace_id": workspace_id.to_string(),
            "import_id": import_id.to_string(),
        }),
        now,
    )
    .with_workspace(workspace_id.to_string());
    JobStore::enqueue_in(transaction, &job)
        .await
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    Ok(true)
}

async fn project_is_connected(
    transaction: &mut Transaction<'_, Any>,
    workspace_id: Id,
    task_id: Id,
) -> Result<bool, sqlx::Error> {
    let Flag(connected) = sqlx::query_scalar::<_, Flag>(
        "SELECT EXISTS(SELECT 1 FROM tasks t \
         JOIN github_project_connections c ON c.project_id = t.project_id \
         AND c.workspace_id = t.workspace_id \
         WHERE t.id = $1 AND t.workspace_id = $2)",
    )
    .bind(task_id.to_string())
    .bind(workspace_id.to_string())
    .fetch_one(&mut **transaction)
    .await?;
    Ok(connected)
}

fn bounded_request_id(request_id: &str) -> String {
    request_id.chars().take(128).collect()
}

/// Canonical content hash shared by inbound and outbound sync: trimmed title, normalized body
/// without the Orbit marker, concatenated and hashed with SHA-256.
#[must_use]
pub fn canonical_content_hash(title: &str, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(title.trim().as_bytes());
    hasher.update(b"\n");
    hasher.update(strip_marker(body).as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Removes the hidden Orbit marker and normalizes line endings.
#[must_use]
pub fn strip_marker(body: &str) -> String {
    body.replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .filter(|line| !is_marker_line(line))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_owned()
}

/// Adds the marker after a blank line, replacing any marker already present.
#[must_use]
pub fn with_marker(description: &str, task_id: Id) -> String {
    let body = strip_marker(description);
    let marker = format!("<!-- orbit-task:{task_id} -->");
    if body.is_empty() {
        marker
    } else {
        format!("{body}\n\n{marker}")
    }
}

fn is_marker_line(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("<!-- orbit-task:") && line.ends_with("-->")
}

fn desired_state(category: &str) -> (&'static str, Option<&'static str>) {
    match category {
        "completed" => ("closed", Some("completed")),
        "cancelled" | "duplicate" => ("closed", Some("not_planned")),
        _ => ("open", None),
    }
}

fn issue_state(issue: &crate::github::client::Issue) -> &'static str {
    if issue.state == "open" {
        "open"
    } else {
        "closed"
    }
}

fn id_field(payload: &Value, name: &str) -> Result<Id, JobError> {
    payload
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| JobError::Permanent(format!("invalid GitHub job payload: missing {name}")))?
        .parse()
        .map_err(|_| JobError::Permanent(format!("invalid GitHub job payload: {name}")))
}

fn request_id_field(payload: &Value) -> String {
    payload
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map_or_else(|| "github-job".to_owned(), bounded_request_id)
}

fn database_error(context: &str, error: sqlx::Error) -> JobError {
    tracing::warn!(%error, context, "GitHub sync database operation failed");
    JobError::Retryable(format!("{context} failed"))
}

fn github_error(error: &GithubError) -> JobError {
    if error.is_retryable() {
        JobError::Retryable(error.sync_message())
    } else {
        JobError::Permanent(error.sync_message())
    }
}

#[derive(Clone)]
pub struct GithubSyncService {
    database: Database,
    client: GithubClient,
    app_key: Option<[u8; 32]>,
}

impl GithubSyncService {
    #[must_use]
    pub fn new(database: Database, app_key: Option<[u8; 32]>) -> Self {
        Self {
            database,
            client: GithubClient::new().with_app_key(app_key),
            app_key,
        }
    }

    #[must_use]
    pub fn with_client(
        database: Database,
        app_key: Option<[u8; 32]>,
        client: GithubClient,
    ) -> Self {
        Self {
            database,
            client: client.with_app_key(app_key),
            app_key,
        }
    }

    #[must_use]
    pub fn database(&self) -> &Database {
        &self.database
    }

    pub fn register_jobs(self, worker: Worker) -> Result<Worker, JobKindRegistrationError> {
        let push = self.clone();
        let pull = self.clone();
        let import = self.clone();
        worker
            .with_handler(push_job_kind(), move |context| {
                let service = push.clone();
                async move {
                    let last_attempt = context.claim.attempt >= context.job.max_attempts;
                    service.run_push(&context.job.payload, last_attempt).await
                }
            })?
            .with_handler(pull_job_kind(), move |context| {
                let service = pull.clone();
                async move {
                    let last_attempt = context.claim.attempt >= context.job.max_attempts;
                    service.run_pull(&context.job.payload, last_attempt).await
                }
            })?
            .with_handler(import_job_kind(), move |context| {
                let service = import.clone();
                async move {
                    let last_attempt = context.claim.attempt >= context.job.max_attempts;
                    service
                        .run_import(&context.job.payload, context.is_cancelled(), last_attempt)
                        .await
                }
            })
    }

    async fn app_credentials(&self, workspace_id: Id) -> Result<Option<AppCredentials>, JobError> {
        if self.app_key.is_none() {
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT app_id, private_key_encrypted FROM github_apps WHERE workspace_id = $1",
        )
        .bind(workspace_id.to_string())
        .fetch_optional(self.database.pool())
        .await
        .map_err(|error| database_error("loading the GitHub App", error))?;
        Ok(row.map(|row| AppCredentials {
            app_id: row.get("app_id"),
            private_key_encrypted: row.get("private_key_encrypted"),
        }))
    }

    async fn run_push(&self, payload: &Value, last_attempt: bool) -> Result<(), JobError> {
        let workspace_id = id_field(payload, "workspace_id")?;
        let task_id = id_field(payload, "task_id")?;
        let actor_id = id_field(payload, "actor_id")?;
        let request_id = request_id_field(payload);
        let force = payload
            .get("force")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let now = TimestampMillis::now();

        let Some(task) = load_task(&self.database, workspace_id, task_id)
            .await
            .map_err(|error| database_error("loading the task", error))?
        else {
            return Ok(());
        };
        if task.deleted {
            return Ok(());
        }
        let Some(connection) = load_connection(&self.database, workspace_id, task.project_id)
            .await
            .map_err(|error| database_error("loading the GitHub connection", error))?
        else {
            return Ok(());
        };
        let link = load_link(&self.database, workspace_id, task_id)
            .await
            .map_err(|error| database_error("loading the GitHub link", error))?;
        if let Some(link) = &link {
            if link.kind != "issue" || link.sync_paused {
                return Ok(());
            }
            if link.sync_conflict && !force {
                return Ok(());
            }
        }

        let local_body = strip_marker(&task.description);
        let local_hash = canonical_content_hash(&task.title, &local_body);
        let (desired, desired_reason) = desired_state(&task.category);
        if let Some(link) = &link
            && !force
            && link.synced_content_hash.as_deref() == Some(local_hash.as_str())
            && link.synced_state.as_deref() == Some(desired)
        {
            return Ok(());
        }
        if task.title.chars().count() > 256 || local_body.chars().count() > 65_536 {
            let message = if task.title.chars().count() > 256 {
                "The task title is longer than GitHub's 256 character limit."
            } else {
                "The task description is longer than GitHub's 65,536 character limit."
            };
            self.record_failure(
                workspace_id,
                task_id,
                Some(actor_id),
                &request_id,
                message,
                now,
            )
            .await;
            return Err(JobError::Permanent(message.to_owned()));
        }

        let Some(app) = self.app_credentials(workspace_id).await? else {
            let message = "The GitHub App is not configured for this workspace.";
            self.record_failure(
                workspace_id,
                task_id,
                Some(actor_id),
                &request_id,
                message,
                now,
            )
            .await;
            return Err(JobError::Permanent(message.to_owned()));
        };

        if let Some(link) = &link {
            return self
                .push_existing(
                    workspace_id,
                    task_id,
                    actor_id,
                    &request_id,
                    force,
                    &task,
                    &local_body,
                    &local_hash,
                    desired,
                    desired_reason,
                    &connection,
                    &app,
                    link,
                    now,
                    last_attempt,
                )
                .await;
        }

        self.push_new(
            workspace_id,
            task_id,
            actor_id,
            &request_id,
            &task,
            &local_body,
            &local_hash,
            desired,
            &connection,
            &app,
            now,
            last_attempt,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn push_existing(
        &self,
        workspace_id: Id,
        task_id: Id,
        actor_id: Id,
        request_id: &str,
        force: bool,
        task: &TaskForPush,
        local_body: &str,
        local_hash: &str,
        desired: &str,
        desired_reason: Option<&str>,
        connection: &Connection,
        app: &AppCredentials,
        link: &LinkRow,
        now: TimestampMillis,
        last_attempt: bool,
    ) -> Result<(), JobError> {
        let issue = match self
            .client
            .get_issue(
                app,
                connection.installation_id,
                &connection.repository,
                link.issue_number,
            )
            .await
        {
            Ok(issue) => issue,
            Err(error) => {
                self.handle_github_failure(
                    workspace_id,
                    task_id,
                    Some(actor_id),
                    request_id,
                    &error,
                    last_attempt,
                    now,
                )
                .await;
                return Err(github_error(&error));
            }
        };
        if issue.is_pull_request() {
            let message = "The linked GitHub item is a pull request.";
            self.record_failure(
                workspace_id,
                task_id,
                Some(actor_id),
                request_id,
                message,
                now,
            )
            .await;
            return Err(JobError::Permanent(message.to_owned()));
        }

        let remote_hash = canonical_content_hash(&issue.title, &issue.body());
        let remote_state = issue_state(&issue);
        let mut synced_hash = link.synced_content_hash.clone();
        let mut synced_state = link.synced_state.clone();

        // A link created before this feature has no snapshot. The first outbound job seeds it
        // from what GitHub currently has, then compares (and records a conflict if needed).
        if synced_hash.is_none() || synced_state.is_none() {
            let seeded_hash = synced_hash.clone().unwrap_or_else(|| remote_hash.clone());
            let seeded_state = synced_state
                .clone()
                .unwrap_or_else(|| remote_state.to_owned());
            sqlx::query(
                "UPDATE github_issue_links SET synced_content_hash = $1, synced_state = $2, \
                 synced_at = COALESCE(synced_at, $3) WHERE workspace_id = $4 AND task_id = $5",
            )
            .bind(&seeded_hash)
            .bind(&seeded_state)
            .bind(now.as_millis())
            .bind(workspace_id.to_string())
            .bind(task_id.to_string())
            .execute(self.database.pool())
            .await
            .map_err(|error| database_error("seeding the GitHub sync snapshot", error))?;
            synced_hash = Some(seeded_hash);
            synced_state = Some(seeded_state);
        }
        let synced_hash = synced_hash.unwrap_or_default();
        let synced_state = synced_state.unwrap_or_default();

        let remote_content_changed = remote_hash != synced_hash;
        let local_content_changed = local_hash != synced_hash;
        let remote_state_changed = remote_state != synced_state;
        let local_state_changed = desired != synced_state;

        if !force {
            if (remote_content_changed && local_content_changed)
                || (remote_state_changed && local_state_changed)
            {
                self.record_conflict(workspace_id, task_id, &remote_hash)
                    .await
                    .map_err(|error| database_error("recording a GitHub conflict", error))?;
                return Ok(());
            }
            if (remote_content_changed && !local_content_changed)
                || (remote_state_changed && !local_state_changed)
            {
                // GitHub changed and Orbit did not: inbound sync applies the remote value.
                return Ok(());
            }
            if !remote_content_changed && !local_content_changed && !local_state_changed {
                return Ok(());
            }
        }

        if !connection.import_all_issues && !connection.label.is_empty() {
            self.ensure_label(connection, app, last_attempt).await?;
        }
        let body = with_marker(local_body, task_id);
        let issue = match self
            .client
            .update_issue(
                app,
                connection.installation_id,
                &connection.repository,
                link.issue_number,
                &task.title,
                &body,
                desired,
                desired_reason,
            )
            .await
        {
            Ok(issue) => issue,
            Err(error) => {
                self.handle_github_failure(
                    workspace_id,
                    task_id,
                    Some(actor_id),
                    request_id,
                    &error,
                    last_attempt,
                    now,
                )
                .await;
                return Err(github_error(&error));
            }
        };

        let mut tx = self
            .database
            .immediate_transaction()
            .await
            .map_err(|error| database_error("updating the GitHub link", error))?;
        sqlx::query(
            "UPDATE github_issue_links SET synced_content_hash = $1, synced_state = $2, \
             synced_at = $3, last_pushed_at = $3, remote_content_hash = NULL, sync_conflict = 0, \
             sync_error = NULL WHERE workspace_id = $4 AND repository = $5 AND issue_number = $6",
        )
        .bind(local_hash)
        .bind(desired)
        .bind(now.as_millis())
        .bind(workspace_id.to_string())
        .bind(&connection.repository)
        .bind(issue.number)
        .execute(&mut *tx)
        .await
        .map_err(|error| database_error("updating the GitHub link", error))?;
        audit::record(
            &mut tx,
            workspace_id,
            Some(actor_id),
            "github.issue.updated",
            AuditOutcome::Success,
            "task",
            Some(task_id),
            request_id,
            json!({
                "repository": connection.repository,
                "issue_number": issue.number,
                "task_id": task_id.to_string(),
            }),
            now,
        )
        .await
        .map_err(|error| database_error("auditing a GitHub update", error))?;
        tx.commit()
            .await
            .map_err(|error| database_error("committing a GitHub update", error))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn push_new(
        &self,
        workspace_id: Id,
        task_id: Id,
        actor_id: Id,
        request_id: &str,
        task: &TaskForPush,
        local_body: &str,
        local_hash: &str,
        desired: &str,
        connection: &Connection,
        app: &AppCredentials,
        now: TimestampMillis,
        last_attempt: bool,
    ) -> Result<(), JobError> {
        if !connection.import_all_issues && !connection.label.is_empty() {
            self.ensure_label(connection, app, last_attempt).await?;
        }
        let labels: Vec<String> = if !connection.import_all_issues && !connection.label.is_empty() {
            vec![connection.label.clone()]
        } else {
            Vec::new()
        };
        let body = with_marker(local_body, task_id);
        let issue = match self
            .client
            .create_issue(
                app,
                connection.installation_id,
                &connection.repository,
                &task.title,
                &body,
                &labels,
            )
            .await
        {
            Ok(issue) => issue,
            Err(error) => {
                self.handle_github_failure(
                    workspace_id,
                    task_id,
                    Some(actor_id),
                    request_id,
                    &error,
                    last_attempt,
                    now,
                )
                .await;
                return Err(github_error(&error));
            }
        };
        if issue.is_pull_request() {
            let message = "GitHub returned a pull request for a created issue.";
            self.record_failure(
                workspace_id,
                task_id,
                Some(actor_id),
                request_id,
                message,
                now,
            )
            .await;
            return Err(JobError::Permanent(message.to_owned()));
        }

        let mut tx = self
            .database
            .immediate_transaction()
            .await
            .map_err(|error| database_error("creating the GitHub link", error))?;
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT task_id FROM github_issue_links WHERE workspace_id = $1 AND task_id = $2",
        )
        .bind(workspace_id.to_string())
        .bind(task_id.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| database_error("re-reading the GitHub link", error))?;

        if existing.is_some() {
            // A webhook won the race and linked the issue through the marker. Do not duplicate
            // the source URL update or the creation audit.
            sqlx::query(
                "UPDATE github_issue_links SET synced_content_hash = $1, synced_state = $2, \
                 synced_at = $3, last_pushed_at = $3, remote_content_hash = NULL, \
                 sync_conflict = 0, sync_error = NULL \
                 WHERE workspace_id = $4 AND repository = $5 AND issue_number = $6",
            )
            .bind(local_hash)
            .bind(desired)
            .bind(now.as_millis())
            .bind(workspace_id.to_string())
            .bind(&connection.repository)
            .bind(issue.number)
            .execute(&mut *tx)
            .await
            .map_err(|error| database_error("updating the GitHub link", error))?;
        } else {
            let inserted = sqlx::query(
                "INSERT INTO github_issue_links (workspace_id, repository, issue_number, task_id, \
                 kind, pull_state, synced_content_hash, synced_state, synced_at, last_pushed_at, \
                 origin) VALUES ($1, $2, $3, $4, 'issue', 'open', $5, $6, $7, $7, 'orbit') \
                 ON CONFLICT DO NOTHING",
            )
            .bind(workspace_id.to_string())
            .bind(&connection.repository)
            .bind(issue.number)
            .bind(task_id.to_string())
            .bind(local_hash)
            .bind(desired)
            .bind(now.as_millis())
            .execute(&mut *tx)
            .await
            .map_err(|error| database_error("creating the GitHub link", error))?;
            if inserted.rows_affected() == 1 {
                sqlx::query(
                    "UPDATE tasks SET source_url = $1, version = version + 1, updated_at = $2 \
                     WHERE id = $3 AND workspace_id = $4",
                )
                .bind(&issue.html_url)
                .bind(now.as_millis())
                .bind(task_id.to_string())
                .bind(workspace_id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(|error| database_error("setting the task source URL", error))?;
                self.add_github_label(&mut tx, workspace_id, task.project_id, task_id, now)
                    .await?;
                audit::record(
                    &mut tx,
                    workspace_id,
                    Some(actor_id),
                    "github.issue.created",
                    AuditOutcome::Success,
                    "task",
                    Some(task_id),
                    request_id,
                    json!({
                        "repository": connection.repository,
                        "issue_number": issue.number,
                        "task_id": task_id.to_string(),
                    }),
                    now,
                )
                .await
                .map_err(|error| database_error("auditing a GitHub creation", error))?;
            }
        }
        tx.commit()
            .await
            .map_err(|error| database_error("committing a GitHub creation", error))?;
        Ok(())
    }

    async fn ensure_label(
        &self,
        connection: &Connection,
        app: &AppCredentials,
        last_attempt: bool,
    ) -> Result<(), JobError> {
        match self
            .client
            .ensure_label(
                app,
                connection.installation_id,
                &connection.repository,
                &connection.label,
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(error) if error.is_retryable() && !last_attempt => Err(github_error(&error)),
            Err(error) => {
                // A missing label must not block creating or updating the issue content.
                tracing::warn!(
                    repository = %connection.repository,
                    "GitHub label could not be ensured: {}",
                    error.sync_message()
                );
                Ok(())
            }
        }
    }

    async fn add_github_label(
        &self,
        tx: &mut Transaction<'_, Any>,
        workspace_id: Id,
        project_id: Id,
        task_id: Id,
        now: TimestampMillis,
    ) -> Result<(), JobError> {
        let label_id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM labels WHERE workspace_id = $1 AND project_id = $2 \
             AND lower(name) = 'github' ORDER BY id LIMIT 1",
        )
        .bind(workspace_id.to_string())
        .bind(project_id.to_string())
        .fetch_optional(&mut **tx)
        .await
        .map_err(|error| database_error("loading the GitHub label", error))?;
        let label_id = match label_id {
            Some(id) => id,
            None => {
                let id = Id::new_v7();
                sqlx::query(
                    "INSERT INTO labels (id, workspace_id, project_id, name, color, version, \
                     created_at, updated_at) VALUES ($1, $2, $3, 'GitHub', '#6e7681', 0, $4, $4) \
                     ON CONFLICT DO NOTHING",
                )
                .bind(id.to_string())
                .bind(workspace_id.to_string())
                .bind(project_id.to_string())
                .bind(now.as_millis())
                .execute(&mut **tx)
                .await
                .map_err(|error| database_error("creating the GitHub label", error))?;
                sqlx::query_scalar(
                    "SELECT id FROM labels WHERE workspace_id = $1 AND project_id = $2 \
                     AND lower(name) = 'github' ORDER BY id LIMIT 1",
                )
                .bind(workspace_id.to_string())
                .bind(project_id.to_string())
                .fetch_optional(&mut **tx)
                .await
                .map_err(|error| database_error("loading the GitHub label", error))?
                .unwrap_or_else(|| id.to_string())
            }
        };
        sqlx::query(
            "INSERT INTO task_labels (task_id, label_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(task_id.to_string())
        .bind(label_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| database_error("adding the GitHub label", error))?;
        Ok(())
    }

    async fn record_conflict(
        &self,
        workspace_id: Id,
        task_id: Id,
        remote_hash: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE github_issue_links SET remote_content_hash = $1, sync_conflict = 1, \
             sync_error = $2 WHERE workspace_id = $3 AND task_id = $4",
        )
        .bind(remote_hash)
        .bind("GitHub changed this issue while the task was edited.")
        .bind(workspace_id.to_string())
        .bind(task_id.to_string())
        .execute(self.database.pool())
        .await?;
        Ok(())
    }

    async fn handle_github_failure(
        &self,
        workspace_id: Id,
        task_id: Id,
        actor_id: Option<Id>,
        request_id: &str,
        error: &GithubError,
        last_attempt: bool,
        now: TimestampMillis,
    ) {
        if !error.is_retryable() || last_attempt {
            self.record_failure(
                workspace_id,
                task_id,
                actor_id,
                request_id,
                &error.sync_message(),
                now,
            )
            .await;
        }
    }

    async fn record_failure(
        &self,
        workspace_id: Id,
        task_id: Id,
        actor_id: Option<Id>,
        request_id: &str,
        message: &str,
        now: TimestampMillis,
    ) {
        if let Err(error) = sqlx::query(
            "UPDATE github_issue_links SET sync_error = $1 WHERE workspace_id = $2 AND task_id = $3",
        )
        .bind(message)
        .bind(workspace_id.to_string())
        .bind(task_id.to_string())
        .execute(self.database.pool())
        .await
        {
            tracing::warn!(%error, "could not record a GitHub sync error");
        }
        let mut tx = match self.database.immediate_transaction().await {
            Ok(tx) => tx,
            Err(error) => {
                tracing::warn!(%error, "could not audit a GitHub sync failure");
                return;
            }
        };
        let _ = audit::record(
            &mut tx,
            workspace_id,
            actor_id,
            "github.issue.sync_failed",
            AuditOutcome::Failure,
            "task",
            Some(task_id),
            request_id,
            json!({ "task_id": task_id.to_string() }),
            now,
        )
        .await;
        let _ = tx.commit().await;
    }

    async fn run_pull(&self, payload: &Value, last_attempt: bool) -> Result<(), JobError> {
        let workspace_id = id_field(payload, "workspace_id")?;
        let task_id = id_field(payload, "task_id")?;
        let actor_id = id_field(payload, "actor_id")?;
        let request_id = request_id_field(payload);
        let now = TimestampMillis::now();

        let Some(task) = load_task(&self.database, workspace_id, task_id)
            .await
            .map_err(|error| database_error("loading the task", error))?
        else {
            return Ok(());
        };
        if task.deleted {
            return Ok(());
        }
        let Some(connection) = load_connection(&self.database, workspace_id, task.project_id)
            .await
            .map_err(|error| database_error("loading the GitHub connection", error))?
        else {
            return Ok(());
        };
        let Some(link) = load_link(&self.database, workspace_id, task_id)
            .await
            .map_err(|error| database_error("loading the GitHub link", error))?
        else {
            return Ok(());
        };
        if link.kind != "issue" || link.sync_paused {
            return Ok(());
        }
        let Some(app) = self.app_credentials(workspace_id).await? else {
            let message = "The GitHub App is not configured for this workspace.";
            self.record_failure(
                workspace_id,
                task_id,
                Some(actor_id),
                &request_id,
                message,
                now,
            )
            .await;
            return Err(JobError::Permanent(message.to_owned()));
        };

        let issue = match self
            .client
            .get_issue(
                &app,
                connection.installation_id,
                &connection.repository,
                link.issue_number,
            )
            .await
        {
            Ok(issue) => issue,
            Err(error) => {
                self.handle_github_failure(
                    workspace_id,
                    task_id,
                    Some(actor_id),
                    &request_id,
                    &error,
                    last_attempt,
                    now,
                )
                .await;
                return Err(github_error(&error));
            }
        };
        if issue.is_pull_request() {
            return Ok(());
        }
        let actor_id = workspace_actor(&self.database, workspace_id, actor_id)
            .await
            .map_err(|error| database_error("choosing the GitHub sync actor", error))?;
        let description = strip_marker(&issue.body());
        let state = issue_state(&issue);
        let synced = TaskRepository::new(self.database.clone())
            .sync_github_work_item(
                workspace_id,
                actor_id,
                GithubWorkItem {
                    repository: connection.repository.clone(),
                    number: link.issue_number,
                    project_id: task.project_id,
                    title: issue.title.clone(),
                    description: description.clone(),
                    kind: "issue",
                    state,
                    state_changed: true,
                },
                &request_id,
                now,
            )
            .await;
        match synced {
            Ok(_) => {}
            Err(TaskError::TrialLimit(reached)) => {
                let message = format!("GitHub sync stopped: {}.", reached.detail());
                self.record_failure(
                    workspace_id,
                    task_id,
                    Some(actor_id),
                    &request_id,
                    &message,
                    now,
                )
                .await;
                return Err(JobError::Permanent(message));
            }
            Err(error) => {
                let retryable = matches!(error, TaskError::Unavailable(_) | TaskError::Busy);
                if !retryable || last_attempt {
                    self.record_failure(
                        workspace_id,
                        task_id,
                        Some(actor_id),
                        &request_id,
                        "Applying the GitHub issue failed.",
                        now,
                    )
                    .await;
                }
                return Err(if retryable {
                    JobError::Retryable("applying the GitHub issue failed".to_owned())
                } else {
                    JobError::Permanent("applying the GitHub issue failed".to_owned())
                });
            }
        }

        let hash = canonical_content_hash(&issue.title, &description);
        sqlx::query(
            "UPDATE github_issue_links SET synced_content_hash = $1, synced_state = $2, \
             synced_at = $3, remote_content_hash = NULL, sync_conflict = 0, sync_error = NULL \
             WHERE workspace_id = $4 AND repository = $5 AND issue_number = $6",
        )
        .bind(hash)
        .bind(state)
        .bind(now.as_millis())
        .bind(workspace_id.to_string())
        .bind(&connection.repository)
        .bind(link.issue_number)
        .execute(self.database.pool())
        .await
        .map_err(|error| database_error("updating the GitHub link", error))?;
        Ok(())
    }

    async fn run_import(
        &self,
        payload: &Value,
        job_cancelled: bool,
        last_attempt: bool,
    ) -> Result<(), JobError> {
        let workspace_id = id_field(payload, "workspace_id")?;
        let import_id = id_field(payload, "import_id")?;
        if job_cancelled {
            self.mark_import(&import_id, "cancelled", None).await;
            return Ok(());
        }
        let now = TimestampMillis::now();
        let Some(import) = load_import(&self.database, workspace_id, import_id)
            .await
            .map_err(|error| database_error("loading the GitHub import", error))?
        else {
            return Ok(());
        };
        if matches!(import.status.as_str(), "completed" | "failed" | "cancelled") {
            return Ok(());
        }

        let Some(connection) = load_connection(&self.database, workspace_id, import.project_id)
            .await
            .map_err(|error| database_error("loading the GitHub connection", error))?
        else {
            self.mark_import(
                &import_id,
                "failed",
                Some("This project is no longer connected to GitHub."),
            )
            .await;
            return Ok(());
        };
        match billing::write_access(&self.database, workspace_id).await {
            Ok(WorkspaceAccess::ReadOnly) => {
                self.mark_import(
                    &import_id,
                    "failed",
                    Some(crate::billing::guard::READ_ONLY_DETAIL),
                )
                .await;
                return Ok(());
            }
            Ok(_) => {}
            Err(error) => {
                return Err(database_error("checking workspace access", error));
            }
        }
        let Some(app) = self.app_credentials(workspace_id).await? else {
            self.mark_import(
                &import_id,
                "failed",
                Some("The GitHub App is not configured for this workspace."),
            )
            .await;
            return Ok(());
        };
        let actor_id = workspace_actor(&self.database, workspace_id, import.requested_by)
            .await
            .map_err(|error| database_error("choosing the GitHub import actor", error))?;
        self.mark_import(&import_id, "running", None).await;

        let label = if connection.import_all_issues {
            None
        } else {
            Some(connection.label.as_str())
        };
        let first = match import.next_cursor {
            Some(cursor) if !cursor.is_empty() => cursor,
            _ => self
                .client
                .repository_issues_url(&connection.repository, label, import.include_closed)
                .map_err(|error| JobError::Permanent(error.sync_message()))?
                .to_string(),
        };
        let mut counters = ImportCounters {
            total: import.total,
            done: import.done,
            created: import.created,
            skipped: import.skipped,
            failed: import.failed,
        };
        let mut cursor = Some(first);
        let request_id = format!("github-import:{import_id}");

        while let Some(page_url) = cursor {
            if import_cancelled(&self.database, import_id)
                .await
                .map_err(|error| database_error("checking GitHub import cancellation", error))?
            {
                self.persist_import(&import_id, &counters, Some(&page_url), now)
                    .await?;
                self.mark_import(&import_id, "cancelled", None).await;
                return Ok(());
            }
            // Keep the current page as the resume point until it is fully applied.
            self.persist_import(&import_id, &counters, Some(&page_url), now)
                .await?;
            let page = match self
                .client
                .list_issues_page(&app, connection.installation_id, &page_url)
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    if error.is_retryable() && !last_attempt {
                        return Err(github_error(&error));
                    }
                    self.mark_import(&import_id, "failed", Some(&error.sync_message()))
                        .await;
                    self.record_import_audit(
                        workspace_id,
                        actor_id,
                        &request_id,
                        "github.issue.import.failed",
                        AuditOutcome::Failure,
                        &counters,
                        now,
                    )
                    .await;
                    return Err(github_error(&error));
                }
            };
            counters.total = counters.total.saturating_add(page.issues.len() as i64);
            for issue in &page.issues {
                if import_cancelled(&self.database, import_id)
                    .await
                    .map_err(|error| database_error("checking GitHub import cancellation", error))?
                {
                    self.persist_import(&import_id, &counters, Some(&page_url), now)
                        .await?;
                    self.mark_import(&import_id, "cancelled", None).await;
                    return Ok(());
                }
                counters.done = counters.done.saturating_add(1);
                if issue.is_pull_request()
                    || (!connection.import_all_issues
                        && !issue.labels().any(|label| label == connection.label))
                {
                    counters.skipped = counters.skipped.saturating_add(1);
                    self.persist_import(&import_id, &counters, Some(&page_url), now)
                        .await?;
                    continue;
                }
                match self
                    .import_issue(
                        workspace_id,
                        import.project_id,
                        &connection.repository,
                        issue,
                        actor_id,
                        &request_id,
                        now,
                    )
                    .await
                {
                    Ok(IssueOutcome::Imported) => {
                        counters.created = counters.created.saturating_add(1);
                    }
                    Ok(IssueOutcome::Skipped) => {
                        counters.skipped = counters.skipped.saturating_add(1);
                    }
                    Err(TaskError::TrialLimit(reached)) => {
                        self.persist_import(&import_id, &counters, Some(&page_url), now)
                            .await?;
                        let message = format!("trial_limit: {}.", reached.detail());
                        self.mark_import(&import_id, "failed", Some(&message)).await;
                        self.record_import_audit(
                            workspace_id,
                            actor_id,
                            &request_id,
                            "github.issue.import.failed",
                            AuditOutcome::Failure,
                            &counters,
                            now,
                        )
                        .await;
                        return Ok(());
                    }
                    Err(TaskError::Unavailable(_) | TaskError::Busy) => {
                        self.persist_import(&import_id, &counters, Some(&page_url), now)
                            .await?;
                        return Err(JobError::Retryable(
                            "importing GitHub issues failed".to_owned(),
                        ));
                    }
                    Err(_) => {
                        counters.failed = counters.failed.saturating_add(1);
                    }
                }
                self.persist_import(&import_id, &counters, Some(&page_url), now)
                    .await?;
            }
            cursor = page.next;
            self.persist_import(&import_id, &counters, cursor.as_deref(), now)
                .await?;
        }

        self.mark_import(&import_id, "completed", None).await;
        self.record_import_audit(
            workspace_id,
            actor_id,
            &request_id,
            "github.issue.import.completed",
            AuditOutcome::Success,
            &counters,
            now,
        )
        .await;
        Ok(())
    }

    async fn import_issue(
        &self,
        workspace_id: Id,
        project_id: Id,
        repository: &str,
        issue: &crate::github::client::Issue,
        actor_id: Id,
        request_id: &str,
        now: TimestampMillis,
    ) -> Result<IssueOutcome, TaskError> {
        let linked: Option<String> = sqlx::query_scalar(
            "SELECT task_id FROM github_issue_links WHERE workspace_id = $1 AND repository = $2 \
             AND issue_number = $3",
        )
        .bind(workspace_id.to_string())
        .bind(repository)
        .bind(issue.number)
        .fetch_optional(self.database.pool())
        .await?;
        if linked.is_some() {
            return Ok(IssueOutcome::Skipped);
        }
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT id FROM tasks t WHERE t.workspace_id = $1 AND t.project_id = $2 \
             AND t.source_url = $3 AND t.deleted_at IS NULL \
             AND NOT EXISTS (SELECT 1 FROM github_issue_links l WHERE l.task_id = t.id) \
             ORDER BY t.created_at, t.id LIMIT 1",
        )
        .bind(workspace_id.to_string())
        .bind(project_id.to_string())
        .bind(&issue.html_url)
        .fetch_optional(self.database.pool())
        .await?;
        if let Some(task_id) = existing {
            sqlx::query(
                "INSERT INTO github_issue_links (workspace_id, repository, issue_number, task_id, \
                 kind, pull_state) VALUES ($1, $2, $3, $4, 'issue', $5) ON CONFLICT DO NOTHING",
            )
            .bind(workspace_id.to_string())
            .bind(repository)
            .bind(issue.number)
            .bind(task_id)
            .bind(issue_state(issue))
            .execute(self.database.pool())
            .await?;
        }
        let description = strip_marker(&issue.body());
        TaskRepository::new(self.database.clone())
            .sync_github_work_item(
                workspace_id,
                actor_id,
                GithubWorkItem {
                    repository: repository.to_owned(),
                    number: issue.number,
                    project_id,
                    title: issue.title.clone(),
                    description,
                    kind: "issue",
                    state: issue_state(issue),
                    state_changed: issue.state == "closed",
                },
                request_id,
                now,
            )
            .await?;
        Ok(IssueOutcome::Imported)
    }

    async fn persist_import(
        &self,
        import_id: &Id,
        counters: &ImportCounters,
        next_cursor: Option<&str>,
        now: TimestampMillis,
    ) -> Result<(), JobError> {
        sqlx::query(
            "UPDATE github_issue_imports SET total = $1, done = $2, created = $3, \
             skipped = $4, failed = $5, next_cursor = $6, updated_at = $7 WHERE id = $8",
        )
        .bind(counters.total)
        .bind(counters.done)
        .bind(counters.created)
        .bind(counters.skipped)
        .bind(counters.failed)
        .bind(next_cursor)
        .bind(now.as_millis())
        .bind(import_id.to_string())
        .execute(self.database.pool())
        .await
        .map_err(|error| database_error("updating the GitHub import", error))?;
        Ok(())
    }

    async fn mark_import(&self, import_id: &Id, status: &str, error: Option<&str>) {
        if let Err(database_error) = sqlx::query(
            "UPDATE github_issue_imports SET status = $1, error = $2, updated_at = $3 \
             WHERE id = $4",
        )
        .bind(status)
        .bind(error)
        .bind(TimestampMillis::now().as_millis())
        .bind(import_id.to_string())
        .execute(self.database.pool())
        .await
        {
            tracing::warn!(%database_error, "could not update the GitHub import status");
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_import_audit(
        &self,
        workspace_id: Id,
        actor_id: Id,
        request_id: &str,
        action: &str,
        outcome: AuditOutcome,
        counters: &ImportCounters,
        now: TimestampMillis,
    ) {
        let mut tx = match self.database.immediate_transaction().await {
            Ok(tx) => tx,
            Err(error) => {
                tracing::warn!(%error, "could not audit a GitHub import");
                return;
            }
        };
        let _ = audit::record(
            &mut tx,
            workspace_id,
            Some(actor_id),
            action,
            outcome,
            "github_import",
            None,
            request_id,
            json!({
                "total": counters.total,
                "done": counters.done,
                "created": counters.created,
                "skipped": counters.skipped,
                "failed": counters.failed,
            }),
            now,
        )
        .await;
        let _ = tx.commit().await;
    }
}

#[derive(Clone, Debug)]
struct TaskForPush {
    project_id: Id,
    title: String,
    description: String,
    category: String,
    deleted: bool,
}

#[derive(Clone, Debug)]
struct Connection {
    installation_id: i64,
    repository: String,
    label: String,
    import_all_issues: bool,
}

#[derive(Clone, Debug)]
struct LinkRow {
    issue_number: i64,
    kind: String,
    sync_paused: bool,
    synced_content_hash: Option<String>,
    synced_state: Option<String>,
    sync_conflict: bool,
}

async fn load_task(
    database: &Database,
    workspace_id: Id,
    task_id: Id,
) -> Result<Option<TaskForPush>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT t.project_id, t.title, t.description, t.deleted_at, s.category \
         FROM tasks t JOIN task_statuses s ON s.id = t.status_id AND s.workspace_id = t.workspace_id \
         WHERE t.id = $1 AND t.workspace_id = $2",
    )
    .bind(task_id.to_string())
    .bind(workspace_id.to_string())
    .fetch_optional(database.pool())
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(TaskForPush {
        project_id: decode_id(&row, "project_id")?,
        title: row.get("title"),
        description: row.get("description"),
        category: row.get("category"),
        deleted: row.get::<Option<i64>, _>("deleted_at").is_some(),
    }))
}

async fn load_connection(
    database: &Database,
    workspace_id: Id,
    project_id: Id,
) -> Result<Option<Connection>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT installation_id, repository, label, import_all_issues \
         FROM github_project_connections WHERE workspace_id = $1 AND project_id = $2",
    )
    .bind(workspace_id.to_string())
    .bind(project_id.to_string())
    .fetch_optional(database.pool())
    .await?;
    Ok(row.map(|row| Connection {
        installation_id: row.get("installation_id"),
        repository: row.get("repository"),
        label: row.get("label"),
        import_all_issues: row.get::<Flag, _>("import_all_issues").0,
    }))
}

async fn load_link(
    database: &Database,
    workspace_id: Id,
    task_id: Id,
) -> Result<Option<LinkRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT issue_number, kind, sync_paused, synced_content_hash, synced_state, \
         sync_conflict FROM github_issue_links WHERE workspace_id = $1 AND task_id = $2",
    )
    .bind(workspace_id.to_string())
    .bind(task_id.to_string())
    .fetch_optional(database.pool())
    .await?;
    Ok(row.map(|row| LinkRow {
        issue_number: row.get("issue_number"),
        kind: row.get("kind"),
        sync_paused: row.get::<Flag, _>("sync_paused").0,
        synced_content_hash: row.get("synced_content_hash"),
        synced_state: row.get("synced_state"),
        sync_conflict: row.get::<Flag, _>("sync_conflict").0,
    }))
}

async fn workspace_actor(
    database: &Database,
    workspace_id: Id,
    preferred: Id,
) -> Result<Id, sqlx::Error> {
    let Flag(is_member) = sqlx::query_scalar::<_, Flag>(
        "SELECT EXISTS(SELECT 1 FROM memberships WHERE workspace_id = $1 AND user_id = $2)",
    )
    .bind(workspace_id.to_string())
    .bind(preferred.to_string())
    .fetch_one(database.pool())
    .await?;
    if is_member {
        return Ok(preferred);
    }
    let owner: Option<String> = sqlx::query_scalar(
        "SELECT user_id FROM memberships WHERE workspace_id = $1 AND role = 'owner' \
         ORDER BY created_at, user_id LIMIT 1",
    )
    .bind(workspace_id.to_string())
    .fetch_optional(database.pool())
    .await?;
    Ok(owner
        .and_then(|owner| owner.parse().ok())
        .unwrap_or(preferred))
}

struct ImportRow {
    status: String,
    include_closed: bool,
    total: i64,
    done: i64,
    created: i64,
    skipped: i64,
    failed: i64,
    next_cursor: Option<String>,
    project_id: Id,
    requested_by: Id,
}

async fn load_import(
    database: &Database,
    workspace_id: Id,
    import_id: Id,
) -> Result<Option<ImportRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT status, include_closed, total, done, created, skipped, failed, next_cursor, \
         project_id, requested_by FROM github_issue_imports WHERE id = $1 AND workspace_id = $2",
    )
    .bind(import_id.to_string())
    .bind(workspace_id.to_string())
    .fetch_optional(database.pool())
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(ImportRow {
        status: row.get("status"),
        include_closed: row.get::<Flag, _>("include_closed").0,
        total: row.get("total"),
        done: row.get("done"),
        created: row.get("created"),
        skipped: row.get("skipped"),
        failed: row.get("failed"),
        next_cursor: row.get("next_cursor"),
        project_id: decode_id(&row, "project_id")?,
        requested_by: decode_id(&row, "requested_by")?,
    }))
}

fn decode_id(row: &sqlx::any::AnyRow, column: &str) -> Result<Id, sqlx::Error> {
    row.get::<String, _>(column)
        .parse()
        .map_err(|_| sqlx::Error::Protocol(format!("invalid identifier in {column}")))
}

async fn import_cancelled(database: &Database, import_id: Id) -> Result<bool, sqlx::Error> {
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM github_issue_imports WHERE id = $1")
            .bind(import_id.to_string())
            .fetch_optional(database.pool())
            .await?;
    Ok(status.as_deref() == Some("cancelled"))
}

#[derive(Clone, Debug)]
struct ImportCounters {
    total: i64,
    done: i64,
    created: i64,
    skipped: i64,
    failed: i64,
}

enum IssueOutcome {
    Imported,
    Skipped,
}

#[cfg(test)]
mod tests {
    use orbit_platform::Id;

    use super::{canonical_content_hash, strip_marker, with_marker};

    #[test]
    fn the_marker_is_added_after_a_blank_line_and_stripped_again() {
        let task = Id::new_v7();
        let body = with_marker("First line\n", task);
        assert!(body.ends_with(&format!("<!-- orbit-task:{task} -->")));
        assert!(body.contains("First line\n\n<!-- orbit-task:"));
        assert_eq!(strip_marker(&body), "First line");
    }

    #[test]
    fn an_empty_description_is_only_the_marker() {
        let task = Id::new_v7();
        let body = with_marker("", task);
        assert_eq!(body, format!("<!-- orbit-task:{task} -->"));
        assert_eq!(strip_marker(&body), "");
    }

    #[test]
    fn canonical_content_normalizes_line_endings_and_trailing_space() {
        let a = canonical_content_hash("Title", "Body\r\n\r\n");
        let b = canonical_content_hash(" Title ", "Body");
        assert_eq!(a, b);
        assert_ne!(a, canonical_content_hash("Title", "Different"));
    }

    #[test]
    fn any_orbit_marker_is_ignored_by_the_hash() {
        let a = canonical_content_hash("Title", "Body");
        let b = canonical_content_hash(
            "Title",
            "Body\n\n<!-- orbit-task:01900000-0000-7000-8000-000000000000 -->",
        );
        assert_eq!(a, b);
    }
}
