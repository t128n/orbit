# GitHub two-way issue sync

- **Date:** 2026-11-08
- **Status:** Draft — the five product decisions below were confirmed by the requester; implementation has not started.
- **Area:** Tasks / Integrations
- **Applies to:** `apps/server`, `apps/web`, `apps/mobile`

**Related:** [`docs/github.md`](../../docs/github.md), [`orbit-cx/docs/github.md`](../docs/github.md), [`orbit-cx/docs/patches.md`](../docs/patches.md)

## 1. Summary

Orbit currently syncs a GitHub issue or pull request into an Orbit task when the issue has the project's connection label. The task's title and description are then read-only, and Orbit never sends task changes back to GitHub.

This spec defines two-way sync for **issue-linked tasks**:

- Orbit creates a GitHub issue for a task when the project is configured to do so, or when a user explicitly asks.
- Later task edits update the issue's title, body, and open/closed state.
- GitHub edits continue to update the task, without echo loops.
- Concurrent edits are detected and surfaced as a conflict instead of silently overwriting either side.
- The connection label becomes optional through an explicit "all issues in this repository" mode.
- Existing GitHub issues can be imported explicitly, never automatically.

Pull requests remain inbound-only.

## 2. Background and current behavior

The existing implementation is verified in:

- `apps/server/src/integration_routes.rs:803` — signed GitHub webhook handler.
- `apps/server/src/integration_routes.rs:1003-1039` — repository/label connection selection.
- `apps/server/src/integration_routes.rs:1081-1099` — calls `TaskRepository::sync_github_work_item`.
- `apps/server/src/repositories/tasks.rs:1846` — creates or updates the task from the GitHub payload.
- `apps/server/migrations/0020_github_integration.sql` — `github_issue_links`, `github_project_connections`, `github_apps`, installations.
- `apps/server/src/repositories/tasks.rs:3166-3183` — refuses title/description edits and project moves for any GitHub-linked task.
- `apps/server/src/collab/kind_task.rs:17-33` — locks the co-edited description for any GitHub-linked task.
- `apps/web/src/features/tasks/components/TaskDetail.tsx:74-82` — marks any GitHub-linked task as read-only.
- `apps/web/src/features/tasks/components/TaskDetail.tsx:398-403` — refuses project moves for linked tasks.
- `apps/web/src/features/tasks/components/ProjectGithubCard.tsx:92-95` — requires a GitHub label.
- `docs/github.md:23` — states the current contract: the task title and description are read-only; Orbit does not send task edits to GitHub.

The current behavior is therefore **label-gated inbound sync with a content lock**, not simply one-way sync.

The only outbound GitHub API call today is the App manifest conversion (`integration_routes.rs:691`). There is no GitHub App issue client.

## 3. Goals

1. Create a GitHub issue from an Orbit task in a connected project.
2. Push later changes to that issue: title, body, and open/closed state.
3. Keep GitHub → Orbit updates working, with echo suppression and conflict detection.
4. Make issue-linked tasks editable in Orbit; keep pull-request-linked tasks read-only.
5. Let a project connect to all issues in a repository without requiring a label, while keeping the labelled mode as the default.
6. Allow a user to import existing GitHub issues explicitly, with progress and idempotency.
7. Keep all outbound GitHub calls off the request path via durable jobs.

## 4. Non-goals

- Two-way sync for pull requests.
- Comment, assignee, milestone, GitHub-label, or sub-issue hierarchy sync.
- Creating GitHub issues as the Orbit user. Issues are created by the GitHub App's bot user.
- Closing or deleting a GitHub issue when an Orbit task is deleted or trashed.
- Automatic backfill of historical GitHub issues or pre-existing Orbit tasks.
- Multiple repositories per project.
- A new GitHub host configuration mechanism. Use the existing configured GitHub web/API origin; the `orbit-cx` polling patch already introduces a configurable host.
- A GitHub Enterprise-specific implementation separate from the standard GitHub App flow.

## 5. Confirmed decisions

Confirmed by the requester on 2026-11-08:

1. **Outbound creation is opt-in per project plus an explicit per-task action.** A new project setting, `push_new_tasks`, defaults to off. With it on, new top-level tasks in the connected project are pushed automatically. Sub-issues stay local unless explicitly pushed. Imports never auto-push.
2. **Conflict policy:** GitHub wins for inbound content when Orbit has not changed locally; Orbit wins for outbound when GitHub has not changed remotely; when both changed, no automatic write happens in either direction and the link is marked conflicted.
3. **Deletion stays local.** Trashing or deleting an Orbit task does not close, delete, or comment on the GitHub issue. Closing the issue is a separate explicit action.
4. **Pull requests stay inbound-only and label-gated**, including when the connection is in all-issues mode.
5. **Import existing issues is explicit.** It runs as a job, is idempotent, and never runs automatically.

## 6. Terminology

- **Connection** — a row in `github_project_connections`: one project, one installed repository, and either a label or all-issues mode.
- **Issue link** — a row in `github_issue_links` with `kind = 'issue'`. This is the only link kind that becomes two-way.
- **Pull-request link** — a row in `github_issue_links` with `kind = 'pull_request'`. It stays inbound-only and read-only.
- **Synced snapshot** — the last title/body/state that both sides agreed on, stored as a content hash and state on the link.
- **Remote observation** — the latest GitHub content or state seen by a webhook or poll but not yet applied.
- **Marker** — a hidden HTML comment in the issue body used to correlate an issue created by Orbit with its task.

## 7. Requirements

### 7.1 Connection settings

`github_project_connections` gains:

- `import_all_issues` (boolean flag, default `0`).
- `push_new_tasks` (boolean flag, default `0`).
- `created_at` (milliseconds, default `0` for existing rows; set on new rows).

Rules:

- `import_all_issues = 0` is the existing behaviour: `label` is required, non-empty, at most 50 characters, and selects which issues sync.
- `import_all_issues = 1`: the connection claims every issue in the repository. `label` is ignored and may be empty or null in the API. Pull requests are still not imported without the label (see 7.10).
- An all-issues connection must be the only connection for that `(workspace_id, repository)`. Saving a second connection for the repository returns `409 github_connection_mode_conflict` with a message asking the user to disconnect the other project first.
- The partial unique index `github_project_connections_all_repo` on `(workspace_id, repository) WHERE import_all_issues = 1` enforces the database side. The API check also rejects an all-issues connection when a labelled connection exists, because the partial index alone cannot express that rule.
- Existing connections keep `import_all_issues = 0` and `push_new_tasks = 0`.

### 7.2 Outbound creation

A GitHub issue is created for a task when:

- The task's project has a connection.
- The task is not in the trash.
- The task has no existing `github_issue_links` row.
- The task is not a pull-request-linked task.
- The connection's `push_new_tasks = 1` and the task was created through a non-import creation path (web UI, public API, or MCP) and is top-level, **or** the user invoked the explicit "Create GitHub issue" action.
- The workspace is not read-only under the cloud billing guard.
- The task did not already have a non-empty `source_url` at creation time. A task with a user-supplied source URL is not auto-pushed; the explicit action remains available and replaces the source URL with the GitHub issue URL.
- The task limit allows one more task. A failed trial-limit check leaves the task local and records no link.

Content:

- Title: the task title.
- Body: the task's markdown `description`, with the marker appended after a blank line.
- Label: the connection label in labelled mode; no label in all-issues mode. Ensure the label exists before creating the issue, or handle GitHub's label validation error.
- The GitHub issue is created by the App's installation token.

On success:

- Insert the `github_issue_links` row with `kind = 'issue'`, `origin = 'orbit'`, the initial synced hash/state, and `synced_at`.
- Set `tasks.source_url` to the issue's HTML URL. This is a versioned task field, so bump `version`; the normal realtime invalidation refreshes open clients.
- Add the project's existing Orbit "GitHub" label to the task, mirroring inbound-created tasks.
- Audit `github.issue.created` with repository, issue number, and task id. Do not include the title or body.
- Clear any previous `sync_error`.

The marker is `<!-- orbit-task:<task_id> -->`. It is stripped from stored task descriptions and re-added on outbound writes if missing.

### 7.3 Outbound updates

When a linked issue task changes, the write transaction enqueues a `github.issue.push` job. The job is enqueued from the repository boundary so all write paths are covered:

- `TaskRepository::create_task` — creation path (see 7.2).
- `TaskRepository::update_task` and `bulk_update_tasks` — title, description, and status changes.
- `project_task_description` — live collaborative description writes.
- Sub-issue automation — an auto-closed parent must also be pushed.
- Explicit "Create GitHub issue" action.
- Conflict resolution "Keep Orbit version".

The worker reads the latest task state rather than trusting the payload. Multiple rapid edits may enqueue several jobs; stale jobs are no-ops because the worker compares the current hash/state with the synced snapshot. Jobs are scheduled a few seconds in the future to coalesce bursts.

Fields pushed:

| Orbit source | GitHub target |
| --- | --- |
| `tasks.title` | issue `title` |
| `tasks.description` (markdown) | issue `body`, excluding the marker |
| status category | issue `state` and `state_reason` |

Not pushed: labels, assignees, priority, due dates, comments, attachments, sub-issue relations, project moves.

Status mapping:

| Orbit status category | GitHub issue |
| --- | --- |
| `unstarted`, `started` | `open` |
| `completed` | `closed`, `state_reason: completed` |
| `cancelled` | `closed`, `state_reason: not_planned` |
| `duplicate` | `closed`, `state_reason: not_planned` |

Before patching, the worker `GET`s the issue and compares the remote content hash and state with the synced snapshot. This catches remote changes that have not arrived as a webhook yet, and avoids a last-writer-wins overwrite.

- If the remote hash and state equal the snapshot, patch normally.
- If the remote content changed and the local content did not, the worker does nothing; inbound sync applies the remote change.
- If both changed, the worker records a conflict and does not patch.
- If the remote issue is gone (`404`/`410`), the link gets `sync_error`; Orbit does not recreate it automatically.

On success, update the synced hash/state, `synced_at`, `last_pushed_at`, clear `remote_content_hash`, `sync_conflict`, and `sync_error`, and audit `github.issue.updated`. Do not update `tasks.updated_at` or `version`; the task content did not change.

### 7.4 Inbound behavior

Inbound continues through `sync_github_work_item`, with these changes:

1. **Marker correlation.** Parse the marker before the normal unlinked-issue path. If it resolves to a live task in the same workspace that has no link and is in a project whose connection matches the repository, link the issue to that task instead of creating a second task. This handles the `issues.opened` webhook arriving before the outbound worker inserts the link.
2. **Connection selection.** For an issue:
   - all-issues connections match every issue in the repository;
   - labelled connections match only issues whose labels include the connection label.
   For a pull request, only labelled connections match. All-issues mode does not import PRs.
3. **Echo suppression.** Compute the remote content hash. If it equals the synced hash, the content did not change since the last agreement; do not overwrite the local title/body. Still process label/pause changes and status changes.
4. **Conflict detection.** If local and remote both differ from the snapshot, store `remote_content_hash` and set `sync_conflict = 1`; do not apply the remote content. The task shows a conflict state.
5. **Status.** Apply the same snapshot/conflict logic to open/closed state. If the issue is closed, map `state_reason = not_planned` to `cancelled` when available; otherwise keep the existing closed → `completed` mapping.
6. **Unlinked closed issues in all-issues mode.** Do not create a task for an unlinked issue whose only activity is `closed`. This prevents closing an old issue from creating a completed task. `opened`, `reopened`, `edited`, and `labeled` may create one.
7. **No outbound echo.** Inbound writes never enqueue an outbound job.

### 7.5 Conflict handling

A conflict exists when both the local task and the GitHub issue changed relative to the synced snapshot. It is represented on the link:

- `sync_conflict = 1`
- `remote_content_hash = <observed remote hash>`
- optionally a human-readable `sync_error`, e.g. "GitHub changed this issue while the task was edited."

While conflicted:

- Outbound jobs skip the link unless they are a forced resolution.
- Inbound webhooks do not overwrite the task; they may update the stored remote hash.
- The task UI shows a **GitHub sync conflict** badge.

Resolution actions:

- **Keep Orbit version** — clear the conflict, enqueue a forced `github.issue.push`; the worker patches the issue with the current task title/body/state.
- **Use GitHub version** — enqueue a `github.issue.pull` job that fetches the issue and applies it through `sync_github_work_item`, bypassing the conflict guard for this explicit resolution.
- Resolution requires the same permission as editing the task.

If no conflict exists, the resolution endpoint returns `409 github_no_conflict`.

### 7.6 Deletion, pause, and connection removal

- Trashing or deleting a task does not change the GitHub issue. The link is kept, and the existing restore behavior remains: a later matching GitHub event can restore the task.
- There is no automatic close or comment on task deletion. A separate explicit "Close GitHub issue" action may be added later; it is not required for the first implementation.
- Removing the connection label in labelled mode sets `sync_paused = 1` on the link, as today. Outbound jobs also skip paused links, so pause stops both directions.
- Removing a project connection or an installation repository sets `sync_paused = 1` for the affected links, or exposes a `connection_missing` state. Outbound jobs skip them. The task remains readable and editable; it just no longer syncs.
- Deleting a task permanently cascades to the link through the existing foreign key. The GitHub issue remains.

### 7.7 Explicit task push

`POST /api/v1/workspaces/{workspace_id}/tasks/{task_id}/github`:

- Requires the caller to be able to edit the task.
- Returns `202` with `{ "queued": true }` after enqueueing a `github.issue.push` job with `reason = manual` and `force = false`.
- Returns `409 github_not_connected` when the project has no connection.
- Returns `409 github_already_linked` when the task already has a link.
- Returns `409 github_pr_linked` when the existing link is a pull request.
- Returns `404` for a task outside the caller's access.
- Returns `409 github_task_trashed` for a task in the trash; restore it first.
- The job creates the issue and performs the same link/audit steps as 7.2.
- If the task already has a non-empty `source_url`, the new GitHub issue URL replaces it; the previous URL is not preserved.

### 7.8 Import existing issues

`POST /api/v1/workspaces/{workspace_id}/projects/{project_id}/github/import`:

Body:

```json
{ "include_closed": false }
```

- Requires workspace `IntegrationsManage` (owner/admin).
- Returns `409 github_import_running` if an import for the project is already queued or running.
- Returns `409 github_not_connected` if the project has no connection.
- Returns `409 read_only_workspace` when the workspace is read-only.
- Creates a `github_issue_imports` row and enqueues a `github.issue.import` job. Returns `202` with the import record.

Import behavior:

- Repository and connection mode come from the project connection.
- Label mode: request only issues with the connection label. All-issues mode: request all issues.
- Use `GET /repos/{owner}/{repo}/issues`, paginating with the `Link` header. Skip entries with a `pull_request` field.
- Default: open issues only. With `include_closed = true`, also fetch closed issues and create completed/cancelled tasks according to `state_reason`.
- Skip issues that already have a `github_issue_links` row.
- If a task in the same project already has the issue's HTML URL as `source_url` and no link, attach the link to that task instead of creating a duplicate; then apply the issue content.
- Create tasks through `sync_github_work_item`, using the import's `requested_by` user as the actor. If that user is no longer a member, fall back to a workspace owner, matching `reconcile_github_attribution`.
- The GitHub service account remains the task creator, as for webhook-created tasks.
- Respect GitHub rate limits. On `429`/`403` with a reset time, stop and retry the job after the reset. On other retryable failures, use the job retry policy.
- If the task trial limit is reached, stop the import with a clear `trial_limit` error and keep the tasks already created.
- Store the next page URL in `next_cursor` so a retried job resumes instead of re-fetching the whole repository.
- Update `total`, `done`, `created`, `skipped`, and `failed` as the job progresses.

Import status:

- `GET /api/v1/workspaces/{workspace_id}/projects/{project_id}/github/import` returns the latest import for the project.
- `POST /api/v1/workspaces/{workspace_id}/projects/{project_id}/github/import/{import_id}/cancel` requests cancellation. The worker checks cancellation between pages and between issues.
- Completed, failed, and cancelled imports remain queryable for the UI.

### 7.9 Project move

Issue-linked tasks still cannot move to another project in the first implementation. Moving would require re-routing the issue to another repository/connection, which is a separate feature. The existing `409 github_linked_move` remains. The UI copy should say the task is synced with GitHub and stays in its project.

### 7.10 Pull requests

- Pull-request links remain inbound-only.
- Their task title and description remain read-only, both through `update_task_in_tx` and the collaborative document lock.
- All-issues mode does not import pull requests. A pull request still requires the connection label.
- No outbound PR calls are made. Editing a PR title/body from Orbit is a separate feature.

### 7.11 Permissions, security, and privacy

- The App manifest requests `issues: write` and `pull_requests: read` instead of `issues: read`. Existing Apps need their permissions updated in GitHub; the manifest does not retroactively change them.
- If the App lacks write permission, the first outbound attempt returns `403`; store a clear `sync_error` ("The GitHub App needs Issues write permission") and show it in project settings and on the task link. Do not retry a permanent permission error.
- The App private key stays encrypted with the server app key. Installation tokens are cached in memory only and never logged or stored in the database.
- Tokens, private keys, request bodies, and task content never appear in logs, audit metadata, or error messages.
- Automatic pushing sends task content to GitHub. It is opt-in per project (`push_new_tasks`) and always available through an explicit action. Project settings copy must say this plainly.
- The marker contains the task UUID. It is an opaque identifier, not a secret. Inbound ignores a marker that resolves to a task in another workspace, a trashed task, or a project without a matching connection.
- The GitHub issue author is the App's bot user (for example `orbit[bot]`), not the Orbit user. This is a documented limitation. Per-user authorship would require user-to-server OAuth tokens and is out of scope.

## 8. Data model

### 8.1 `github_project_connections`

Add:

```sql
ALTER TABLE github_project_connections ADD COLUMN import_all_issues INTEGER NOT NULL DEFAULT 0;
ALTER TABLE github_project_connections ADD COLUMN push_new_tasks INTEGER NOT NULL DEFAULT 0;
ALTER TABLE github_project_connections ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
```

Postgres uses `BIGINT` for the flag columns. Mirror the migration under `apps/server/migrations/postgres/`.

Add the partial unique index:

```sql
CREATE UNIQUE INDEX github_project_connections_all_repo
    ON github_project_connections (workspace_id, repository)
    WHERE import_all_issues = 1;
```

### 8.2 `github_issue_links`

Add:

```sql
ALTER TABLE github_issue_links ADD COLUMN synced_content_hash TEXT;
ALTER TABLE github_issue_links ADD COLUMN synced_state TEXT;
ALTER TABLE github_issue_links ADD COLUMN synced_at INTEGER;
ALTER TABLE github_issue_links ADD COLUMN remote_content_hash TEXT;
ALTER TABLE github_issue_links ADD COLUMN sync_conflict INTEGER NOT NULL DEFAULT 0;
ALTER TABLE github_issue_links ADD COLUMN sync_error TEXT;
ALTER TABLE github_issue_links ADD COLUMN last_pushed_at INTEGER;
ALTER TABLE github_issue_links ADD COLUMN origin TEXT NOT NULL DEFAULT 'github';
```

`origin` is `'github'` for existing and webhook-created links and `'orbit'` for issues created by Orbit. It is diagnostic/UI metadata, not authoritative for sync.

`synced_content_hash` is the SHA-256 hex digest of the canonical title + body. `remote_content_hash` is the latest remote hash observed but not applied, used for conflict detection.

### 8.3 `github_issue_imports`

```sql
CREATE TABLE github_issue_imports (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    repository TEXT NOT NULL,
    requested_by TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'completed', 'failed', 'cancelled')),
    include_closed INTEGER NOT NULL DEFAULT 0 CHECK (include_closed IN (0, 1)),
    total INTEGER NOT NULL DEFAULT 0 CHECK (total >= 0),
    done INTEGER NOT NULL DEFAULT 0 CHECK (done >= 0),
    created INTEGER NOT NULL DEFAULT 0 CHECK (created >= 0),
    skipped INTEGER NOT NULL DEFAULT 0 CHECK (skipped >= 0),
    failed INTEGER NOT NULL DEFAULT 0 CHECK (failed >= 0),
    next_cursor TEXT,
    error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX github_issue_imports_project ON github_issue_imports (project_id, created_at DESC);
CREATE INDEX github_issue_imports_status ON github_issue_imports (status, updated_at);
```

Postgres uses `BIGINT` for the counters and flags. `next_cursor` stores the next GitHub page URL, not a page number, so it works with GitHub's `Link` pagination.

### 8.4 Migration numbering

Use the next free migration number when implementing. The current SQLite/Postgres trees end at `0077`; the `orbit-cx` polling patch set reserves `0079`. If both are kept, use `0080` to avoid a collision. Do not rewrite an applied migration.

## 9. API

### 9.1 Update project connection

`PUT /api/v1/workspaces/{workspace_id}/projects/{project_id}/github`

```json
{
  "installation_id": 1234,
  "repository": "owner/repo",
  "label": "Orbit",
  "import_all_issues": false,
  "push_new_tasks": false
}
```

- `label` is required when `import_all_issues` is false and ignored when true.
- Unknown fields are rejected, as today.
- Validation errors use `422 invalid_github_connection`.
- Mode conflict uses `409 github_connection_mode_conflict`.
- The existing label conflict (`409 github_label_conflict`) remains for labelled mode.

Response and `GET` settings gain:

```json
{
  "import_all_issues": false,
  "push_new_tasks": false
}
```

### 9.2 Links

`GET /api/v1/workspaces/{workspace_id}/tasks/{task_id}/github-links` gains:

```json
{
  "kind": "issue",
  "title": "owner/repo#12",
  "url": "https://github.com/owner/repo/issues/12",
  "state": "active",
  "source": true,
  "origin": "orbit",
  "sync_error": null,
  "sync_conflict": false,
  "last_synced_at": 1762646400000
}
```

`state` is `paused` when `sync_paused = 1`, `conflict` when `sync_conflict = 1`, `error` when `sync_error` is set, otherwise `active` for issues. Pull-request links keep the existing `pull_state` values. A non-null `sync_error` may accompany any state.

### 9.3 Task push and conflict resolution

- `POST /api/v1/workspaces/{workspace_id}/tasks/{task_id}/github` — enqueue an issue creation. `202`.
- `POST /api/v1/workspaces/{workspace_id}/tasks/{task_id}/github/resolve` — body `{ "resolution": "orbit" | "github" }`. `202`.
- Both require task edit access.
- New error codes: `github_not_connected`, `github_already_linked`, `github_pr_linked`, `github_no_conflict`, `github_task_trashed`.

### 9.4 Import

- `POST /api/v1/workspaces/{workspace_id}/projects/{project_id}/github/import` — body `{ "include_closed": false }`, `202` with the import record.
- `GET /api/v1/workspaces/{workspace_id}/projects/{project_id}/github/import` — latest import or `404`.
- `POST /api/v1/workspaces/{workspace_id}/projects/{project_id}/github/import/{import_id}/cancel` — `202`.
- All require `IntegrationsManage`.
- New error codes: `github_import_running`, `github_import_not_found`, `github_not_connected`.

All route changes require regenerating the OpenAPI client (`just api` / `just openapi`) and updating `packages/api/src/generated`.

## 10. Jobs

New job kinds, registered in `apps/server/src/app.rs` through the existing worker:

| Kind | Purpose | Default policy |
| --- | --- | --- |
| `github.issue.push` | Create or patch one issue | 8 attempts, standard retry delays, 5-minute lease |
| `github.issue.pull` | Fetch one issue and apply it after a "Use GitHub version" resolution | 8 attempts |
| `github.issue.import` | Paginate and import issues | 8 attempts, cancellable between pages and issues |

Payloads contain ids and flags only:

```json
// github.issue.push
{ "workspace_id": "...", "task_id": "...", "actor_id": "...", "request_id": "...", "reason": "created|updated|manual|resolve", "force": false }

// github.issue.pull
{ "workspace_id": "...", "task_id": "...", "actor_id": "...", "request_id": "..." }

// github.issue.import
{ "workspace_id": "...", "import_id": "..." }
```

Do not put titles, bodies, tokens, or App keys in job payloads. Mark the payload redacted for logging.

The push job is idempotent:

- If a link appears while the job was creating an issue, re-read the link and continue as an update.
- If the issue was already created and the marker is present, do not create a second issue.
- Stale jobs are no-ops because the worker compares the current task hash/state with the link snapshot.
- A job whose task was deleted or whose connection was removed exits successfully without side effects.

## 11. Sync algorithm

### 11.1 Canonical content

`canonical_content(title, body)`:

1. Trim the title.
2. Normalize line endings in the body to `\n`.
3. Remove the marker line from the body.
4. Trim trailing whitespace from the body.
5. Concatenate `title + "\n" + body`.
6. Return the lowercase hex SHA-256 digest.

The same function is used by inbound and outbound so echo detection is symmetric.

### 11.2 Inbound decision table

Let `S` be `synced_content_hash`, `L` the current local hash, and `R` the incoming remote hash.

If `S` is null (a link created before this feature), the first inbound event seeds the snapshot from the current GitHub content and then applies it, which is the current behavior. The first outbound job seeds the snapshot from the current GitHub issue; if the local content differs at that point, it records a conflict instead of overwriting either side. A null `synced_state` is seeded the same way.

| Condition | Action |
| --- | --- |
| `R = S` | No remote content change. Do not touch the title/body. |
| `L = S` and `R ≠ S` | Apply the remote content. Set `S = R`, clear `remote_content_hash`. |
| `L ≠ S` and `R = S` | Local change not yet pushed. Do not apply remote content. |
| `L ≠ S` and `R ≠ S` | Conflict. Store `remote_content_hash = R`, set `sync_conflict = 1`, leave the task unchanged. |

Status uses the same table with `synced_state`, local desired state, and remote state. Content and status are evaluated independently; either can set the conflict flag.

### 11.3 Outbound decision table

1. If `sync_conflict = 1` and `force = false`, exit.
2. Compute `L` and the desired state.
3. If `S` is not null, `L = S`, and desired state = `synced_state`, exit (no-op).
4. `GET` the issue.
5. Compute `R` from the issue body/title and the remote state. If `S` is null, set `S = R` and `synced_state` to the remote state before comparing.
6. If `R ≠ S` and `L ≠ S`, or remote state ≠ `synced_state` and desired state ≠ `synced_state`, set the conflict and exit.
7. If `R ≠ S` and `L = S`, exit; inbound will apply the remote change.
8. Otherwise `PATCH` the issue with the desired title, body (plus marker), and state.
9. On success, set `S = L`, `synced_state = desired`, `synced_at`, `last_pushed_at`, clear `remote_content_hash`, `sync_conflict`, and `sync_error`.
10. Audit `github.issue.updated`.

For a forced "Keep Orbit version" resolution, skip the conflict checks in steps 1, 6, and 7, but still `GET` for diagnostics if desired. Patch with the local content.

### 11.4 Create algorithm

1. Load the task, connection, App credentials, and installation id.
2. If the task is trashed, not connected, already linked, a PR link, or the workspace is read-only, exit successfully or with the documented API error (for the explicit action).
3. Obtain an installation token (cached until expiry).
4. Ensure the connection label exists in labelled mode.
5. `POST /repos/{owner}/{repo}/issues` with title, body + marker, and optional label.
6. In an immediate transaction:
   - Re-read the link. If one appeared (webhook race), continue as an update instead of inserting.
   - Insert `github_issue_links` with `origin = 'orbit'`, `synced_content_hash`, `synced_state`, `synced_at`.
   - Set `tasks.source_url`, bump `version`, and update `updated_at`.
   - Add the Orbit "GitHub" label if absent.
   - Audit `github.issue.created`.
7. Commit.
8. If the webhook already linked the issue via the marker, the transaction step sees the existing link and must not duplicate the source URL update or audit.

### 11.5 Import algorithm

1. Load the import, connection, App credentials, and installation id.
2. If the import is cancelled, mark it cancelled and exit.
3. Build the first page URL from `next_cursor` or the repository issues endpoint.
4. For each page:
   - Parse the `Link` header for the next page.
   - For each issue:
     - Skip pull requests.
     - Skip issues that already have a link.
     - Skip labelled-mode issues without the label (normally handled by the API query, but check anyway).
     - Link an existing task with the same `source_url` if found.
     - Otherwise call `sync_github_work_item` with the issue title, body, kind, state, and `state_changed = true` for closed issues.
     - Update counters.
     - Check cancellation between issues.
   - Persist `next_cursor` and counters before fetching the next page.
5. On completion, mark the import completed and audit the result.
6. On failure, record the error and mark failed; retryable errors leave the job retryable and keep `next_cursor`.

Rate-limit handling: a `429`, or a `403` with `X-RateLimit-Remaining: 0`, pauses until `Retry-After` or `X-RateLimit-Reset`. Do not spin.

## 12. Error handling

| GitHub response | Treatment | Link state |
| --- | --- | --- |
| `401` | Permanent; App credentials invalid | `sync_error` |
| `403` rate limit | Retry after reset | unchanged |
| `403` permission | Permanent; missing Issues write or installation access | `sync_error` with permission guidance |
| `404`/`410` | Permanent; issue deleted or inaccessible | `sync_error` |
| `422` | Permanent; validation (title/body too long, bad label) | `sync_error` with a safe field summary |
| `429` | Retry after reset | unchanged |
| `5xx`, timeout, network | Retry with backoff | `sync_error` only after final failure |

Permanent errors are not retried. Retryable errors use the job retry policy. When a job reaches its final attempt, set `sync_error` and audit `github.issue.sync_failed` without content.

Limits:

- GitHub caps issue titles at 256 characters and issue bodies at 65,536. Orbit allows 500 and 100,000 respectively. Decide at implementation time whether to reject the push with a clear `sync_error` or truncate. Rejecting is safer; do not silently truncate user content.
- The connection label is at most 50 characters, matching the existing validation.

Workspace read-only:

- The cloud billing read-only guard applies to outbound creation, imports, and conflict resolution. Push jobs that were already queued should also stop when the workspace becomes read-only. Reuse the existing `READ_ONLY_DETAIL` problem.

## 13. UI and documentation

Web:

- `ProjectGithubCard`: add the mode selector ("Only issues with label X" / "All issues in this repository"), show the label field only in labelled mode, add the `push_new_tasks` toggle, add **Import existing issues** with an `include_closed` checkbox and a warning that it creates tasks, and show the latest import progress/status.
- `TaskDetail`: issue-linked tasks are editable. Pull-request-linked tasks stay read-only with the existing explanation. Show badges for `paused`, `conflict`, and `sync_error`; add **Create GitHub issue** when eligible; add **Keep Orbit version** / **Use GitHub version** when conflicted; keep the project-move lock and its copy.
- Update the GitHub links section copy to mention two-way sync for issues and inbound-only for PRs.
- Regenerate the API client and update frontend query invalidation if new keys are introduced.

Mobile:

- The mobile app has no project GitHub settings screen today. Project connection settings remain web-only; note that in the spec and the mobile settings copy.
- Port the task-level changes the app can show: create-issue action in the task menu when eligible, and the `paused`/`conflict`/`sync_error` badges or status text. Verify with `bunx tsc --noEmit -p .` and `bun test src` in `apps/mobile`.
- Pull-request-linked tasks remain read-only where the mobile editor currently allows edits; the server remains authoritative.

Documentation:

- `docs/github.md`: replace the "read-only / never sends edits" contract with two-way behavior, the opt-in setting, the label/all-issues modes, conflicts, deletion semantics, import, permissions, and the bot-author limitation.
- `orbit-cx/docs/github.md`: document the same behavior in the fork, including the configurable host and polling mode interaction.
- `.ai/FEATURES.md`: update the statement that GitHub-linked tasks are locked.
- `orbit-cx/README.md` / `orbit-cx/docs/patches.md`: add the feature to the patch inventory only when it becomes a patch set.

## 14. Rollout and compatibility

- Existing connections keep the current labelled behavior. `push_new_tasks` and `import_all_issues` default to off.
- Existing links get `origin = 'github'` and no synced snapshot. On the first inbound event, the worker can populate the snapshot from the current GitHub content. Until then, it behaves like today.
- Existing GitHub Apps have `issues: read`. Outbound writes fail until the App owner changes the permission to read-and-write and, if prompted, installation owners approve the change. `start_github_manifest` cannot register a second App for a workspace, so the spec does not require a re-registration flow; document the manual permission change and surface the `403` clearly.
- The fork's configurable GitHub host must be used for both the web and API base URLs. Do not hardcode `https://api.github.com` in the new client.
- Polling mode and webhook mode must share the same inbound apply path and conflict logic. The outbound client is transport-independent.

## 15. Testing and acceptance criteria

Automated server tests (SQLite and Postgres where the behaviour is database-dependent):

1. Creating a task in a connected project with `push_new_tasks = 1` calls `POST /issues`, inserts a link, sets `source_url`, and audits.
2. Creating a task with `push_new_tasks = 0` does not call GitHub until the explicit action is used.
3. A non-import task with a non-empty `source_url` is not auto-pushed.
4. Updating title, description, and status calls the expected `PATCH` with the marker and label.
5. A collaborative description flush enqueues exactly one meaningful push and stale jobs are no-ops.
6. Closing a task sends `state: closed, state_reason: completed`; cancelling sends `not_planned`; reopening sends `open`.
7. A webhook echo after a push does not overwrite the task (hash suppression).
8. A webhook with a remote change and no local change applies it.
9. Both changed produces a conflict, no write; "Keep Orbit version" force-pushes; "Use GitHub version" pulls.
10. `issues.opened` arriving before the link insert does not create a duplicate task (marker correlation).
11. A missing Issues write permission produces `sync_error`, no data loss, and no retry storm.
12. Deleting a task does not call GitHub; the link remains and the task can be restored by a later event.
13. A paused link skips outbound jobs.
14. A pull-request-linked task rejects title/description edits and is not pushed.
15. Labelled mode ignores unlabelled issues; all-issues mode syncs them; PRs remain label-gated in both modes.
16. Two all-issues connections for one repository are rejected; an all-issues connection plus a labelled connection is rejected.
17. Import: open issues only by default; `include_closed` includes closed; PRs skipped; linked issues skipped; `source_url` duplicates linked, not duplicated; cursor resume works; trial limit stops cleanly; cancellation works.
18. Import requires `IntegrationsManage`; task push requires task edit access.
19. Existing label-mode behavior is unchanged.
20. OpenAPI contract passes; generated API client is current.

Web tests:

- Project settings mode selector, label visibility, toggles, validation, and import dialog/progress.
- Issue-linked task title/description editing; PR-linked task stays read-only.
- Create-issue action visibility and error cases.
- Conflict/error/paused badges and resolution actions.
- Existing GitHub settings/paused tests continue to pass.

Mobile:

- `bunx tsc --noEmit -p .` and `bun test src` in `apps/mobile`.
- Task-level create action and status badges where the screen exists.
- The iOS bundle builds on the running Metro server.

Manual verification:

- Register a GitHub App (or update an existing one) with Issues read-and-write.
- Connect a test repository in labelled and all-issues modes.
- Create, edit, close, and reopen a task; verify GitHub and Orbit converge.
- Edit the issue on GitHub; verify the task updates.
- Force a conflict by editing both sides; verify the resolution actions.
- Run an explicit import against a repository with open, closed, labelled, unlabelled, and PR entries.
- Confirm no tokens, bodies, or private keys appear in logs or audit metadata.

## 16. Assumptions and open questions

Assumptions verified in discussion or grounded in the current code:

- Auto-push applies to top-level tasks created through a non-import path only.
- A task with a user-supplied `source_url` is not auto-pushed.
- Closed issues imported with `include_closed` become completed tasks; `state_reason = not_planned` may map to cancelled.
- Imported and webhook-created tasks use the existing GitHub service account as creator.
- The project-move refusal stays for issue-linked tasks in the first implementation.

Open questions for implementation:

- Should the explicit "Create GitHub issue" action be available to all task editors or only `IntegrationsManage`? This spec assumes task editors, with the project setting carrying the external-sharing decision.
- Should "Close GitHub issue" be a first-class action, or only "Use GitHub version"/unlink? The spec keeps closing out of the deletion path and leaves a separate action optional.
- Should outbound title/body limit violations be rejected or truncated? This spec recommends rejecting with a `sync_error`.
- Should an all-issues connection record a `created_at` cutoff so old issues edited after connection setup are treated differently? The spec starts from connection creation for new events; no historical backfill happens without the explicit import.
- Should a reconciliation job periodically compare linked issues? It is useful hardening but not required for the first implementation.

## 17. References

- GitHub App authentication: JWT with RS256, installation access tokens.
- GitHub REST: create issue, update issue, list repository issues, labels.
- Existing implementation: `apps/server/src/integration_routes.rs`, `apps/server/src/repositories/tasks.rs`.
- Existing job infrastructure: `crates/platform/src/jobs`.
- `orbit-cx/patches/0002-pr2-feat-github-polling-sync.patch` contains a working App JWT/installation-token implementation and rate-limit structure that can be reused instead of adding a new JWT crate.
