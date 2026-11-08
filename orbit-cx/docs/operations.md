# Operations & Deployment (Orbit-CX)

This guide documents operational requirements and configuration variables for running Orbit-CX instances.

---

## Environment Variables

| Variable | Default | Purpose |
|---|---|---|
| `ORBIT_GITHUB_URL` | `https://github.com` | Target GitHub origin for OAuth and GitHub Apps. |
| `ORBIT_APP_KEY` | *(none)* | **Required.** 32-byte base64-encoded secret key for encrypting app secrets and private keys. |
| `ORBIT_PUBLIC_ORIGIN` | *(none)* | Canonical public origin for Orbit (required for webhook mode, optional for polling). |
| `ORBIT_DATABASE_URL` | *(none)* | PostgreSQL database connection string (if using Postgres instead of SQLite). |
| `ORBIT_DATABASE_PATH` | `./data/orbit.sqlite` | SQLite database path when running with default SQLite engine. |

---

## Database Migrations

Orbit-CX maintains migrations for both SQLite (`apps/server/migrations/`) and Postgres (`apps/server/migrations/postgres/`):

- `0078_github_host.sql`: Adds `github_host` columns to sign-in provider and integration records.
- `0079_github_polling.sql`: Adds polling schedules, sync states, and cursor tracking tables.

---

## Host Binding Protection

To prevent split-brain states where tasks or integrations mix GitHub.com and GitHub Enterprise data:
1. Upon initializing GitHub integrations, Orbit writes the normalized host to the database metadata table.
2. Every subsequent startup compares `ORBIT_GITHUB_URL` against this record.
3. If a mismatch is detected, startup halts with an explicit error to prevent cross-host corruption.
