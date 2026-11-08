# Operate an Orbit-CX instance

An Orbit-CX server needs the same configuration as upstream Orbit, plus settings for the GitHub host. This page lists the variables the patch set adds or changes, the migrations it adds, and the startup check that keeps a database on one host.

## GitHub environment variables

| Variable | Default | Purpose |
|---|---|---|
| `ORBIT_GITHUB_URL` | `https://github.com` | GitHub web origin for sign-in, OAuth callbacks, and workspace Apps. The database binds to one host, and changing hosts is unsupported. |
| `ORBIT_APP_KEY` | none | Required. Orbit encrypts saved App secrets and private keys with this key: base64 of 32 random bytes. |
| `ORBIT_APP_KEY_FILE` | none | Path to a file that holds the app key, instead of `ORBIT_APP_KEY`. Do not set both. |
| `ORBIT_PUBLIC_ORIGIN` | none | Canonical public origin of the instance. Required in production, and always for webhook mode. |
| `ORBIT_DATABASE_URL` | none | Postgres connection string. Set it to run on Postgres instead of SQLite. |
| `ORBIT_DATABASE_PATH` | `./data/orbit.sqlite` | SQLite database file. Used when `ORBIT_DATABASE_URL` is unset. |

Keep a copy of the app key outside the server and outside your database backups. Without it, Orbit cannot read the GitHub App private key it stored, and App authentication fails.

For the full variable list, see the upstream [operations documentation](../../docs/operations.md).

## Database migrations

Patches 0001 and 0002 add two migrations, each with a SQLite and a Postgres copy. Orbit runs each copy through the same migration runner, so a database on either engine ends at schema version 79.

| Migration | Tables | Contents |
|---|---|---|
| `0078_github_host.sql` | `github_host_binding` | One row with the normalized GitHub origin |
| `0079_github_polling.sql` | `github_workspace_settings` | Sync mode and poll interval per workspace |
| | `github_poll_state` | Poll generation, lease, next poll time, backoff, rate-limit horizon |
| | `github_poll_transition` | Pending mode handoff and its fencing values |
| | `github_poll_page_cache` | Cached API pages for incremental polls |

`apps/server/migrations/` holds the SQLite files and `apps/server/migrations/postgres/` holds the Postgres files. When you rebase onto upstream, a new upstream migration number can collide with `0078` or `0079`. Renumber both copies of this patch set, then update the `schema_migrations` version assertion in the platform tests.

## Host binding at startup

Orbit compares the configured host with the database on every startup, so a database does not mix records from two hosts:

1. The first time the GitHub integration runs, Orbit writes the normalized origin to `github_host_binding`.
2. Later startups compare `ORBIT_GITHUB_URL` with that row.
3. On a mismatch, startup stops with a configuration error and names the expected origin.

Orbit binds existing databases that already contain GitHub records to `https://github.com`. To recover from a failed startup, restore the value the database expects, or point the server at the database that matches the value you set.

## Upgrade a deployment before you change hosts

Old Orbit binaries do not read the binding table, so they can write GitHub records for a different host into a shared database. Before you switch to a non-default host:

1. Stop every server that shares the database.
2. Upgrade each one to a build with this patch set applied.
3. Set `ORBIT_GITHUB_URL` on every server, with the same value.
4. Start one server and confirm it reaches the setup screen.
5. Start the remaining servers.

A rolling upgrade cannot protect the database during step 2, because the old instances keep running while the new ones start.
