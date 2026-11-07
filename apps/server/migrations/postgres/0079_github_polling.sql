CREATE TABLE github_workspace_settings (
    workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    sync_mode TEXT NOT NULL DEFAULT 'webhooks' CHECK (sync_mode IN ('webhooks', 'polling')),
    poll_interval_minutes BIGINT NOT NULL DEFAULT 5 CHECK (poll_interval_minutes BETWEEN 1 AND 15),
    config_version BIGINT NOT NULL DEFAULT 1,
    mode_epoch BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE github_poll_state (
    workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    generation BIGINT NOT NULL DEFAULT 0,
    config_version BIGINT NOT NULL DEFAULT 0,
    lease_owner TEXT,
    lease_expires_at BIGINT,
    next_poll_at BIGINT,
    rate_limit_not_before BIGINT,
    backoff_until BIGINT,
    consecutive_failures BIGINT NOT NULL DEFAULT 0,
    last_started_at BIGINT,
    last_finished_at BIGINT,
    last_status TEXT,
    last_result TEXT,
    last_items_seen BIGINT NOT NULL DEFAULT 0,
    last_items_updated BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE github_poll_transition (
    workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    target_mode TEXT NOT NULL CHECK (target_mode IN ('webhooks', 'polling')),
    config_version BIGINT NOT NULL,
    mode_epoch BIGINT NOT NULL,
    minimum_generation BIGINT NOT NULL,
    poll_interval_minutes BIGINT NOT NULL CHECK (poll_interval_minutes BETWEEN 1 AND 15)
);

CREATE TABLE github_poll_page_cache (
    workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    config_version BIGINT NOT NULL,
    pages_encrypted BYTEA NOT NULL
);
