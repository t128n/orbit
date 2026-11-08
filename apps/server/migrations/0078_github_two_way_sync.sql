-- GitHub two-way issue sync: connection modes, link snapshots and explicit imports.

ALTER TABLE github_project_connections ADD COLUMN import_all_issues INTEGER NOT NULL DEFAULT 0;
ALTER TABLE github_project_connections ADD COLUMN push_new_tasks INTEGER NOT NULL DEFAULT 0;
ALTER TABLE github_project_connections ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;

-- All-issues mode claims every issue in a repository, so one repository has at most one such
-- connection. Labelled connections are still governed by the existing unique constraint.
CREATE UNIQUE INDEX github_project_connections_all_repo
    ON github_project_connections (workspace_id, repository)
    WHERE import_all_issues = 1;

ALTER TABLE github_issue_links ADD COLUMN synced_content_hash TEXT;
ALTER TABLE github_issue_links ADD COLUMN synced_state TEXT;
ALTER TABLE github_issue_links ADD COLUMN synced_at INTEGER;
ALTER TABLE github_issue_links ADD COLUMN remote_content_hash TEXT;
ALTER TABLE github_issue_links ADD COLUMN sync_conflict INTEGER NOT NULL DEFAULT 0;
ALTER TABLE github_issue_links ADD COLUMN sync_error TEXT;
ALTER TABLE github_issue_links ADD COLUMN last_pushed_at INTEGER;
ALTER TABLE github_issue_links ADD COLUMN origin TEXT NOT NULL DEFAULT 'github';

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
