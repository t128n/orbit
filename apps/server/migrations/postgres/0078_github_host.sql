CREATE TABLE github_host_binding (
    singleton BIGINT PRIMARY KEY CHECK (singleton = 1),
    origin TEXT NOT NULL
);
