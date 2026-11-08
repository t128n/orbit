# GitHub Integration (Orbit-CX)

Orbit-CX extends upstream Orbit's GitHub App integration to support **self-hosted GitHub Enterprise Server / Cloud instances** and **inbound-free Polling Sync**.

---

## 1. Configurable GitHub Host

Upstream Orbit is hardcoded to `https://github.com`. Orbit-CX introduces `ORBIT_GITHUB_URL` to select any GitHub web origin for authentication, OAuth callbacks, and workspace GitHub Apps.

### Supported Host Types

| Web Origin | Detected Type | REST API Base | GraphQL Endpoint |
|---|---|---|---|
| `https://github.com` (default) | GitHub.com | `https://api.github.com` | `https://api.github.com/graphql` |
| `https://acme.ghe.com` | GitHub Enterprise Cloud (Data Residency) | `https://api.acme.ghe.com` | `https://api.acme.ghe.com/graphql` |
| `https://github.example.com` | GitHub Enterprise Server | `https://github.example.com/api/v3` | `https://github.example.com/api/graphql` |

### Configuration Rules
- Set `ORBIT_GITHUB_URL` to an HTTPS origin without credentials, path, query, or fragment. An optional port is permitted.
- The database binds to one GitHub host. Databases with existing GitHub records are locked to their initial host. Changing hosts on an existing database is prohibited to prevent data corruption.
- Older Orbit binaries ignore host-binding locks; ensure all instances sharing a database are running Orbit-CX before configuring custom hosts.

---

## 2. Workspace Polling Mode

By default, GitHub sends real-time webhook events to Orbit's public HTTPS URL. However, instances running behind corporate firewalls, VPNs, or private subnets cannot receive inbound webhooks without complex tunnel setups.

Orbit-CX provides a **workspace-level polling sync mode**:

- **No Public Webhooks Required:** In polling mode, the GitHub App manifest omits webhook configuration (`hook_attributes`) and requests no webhook events. GitHub does not validate Internet reachability.
- **Outbound-Only Sync:** Orbit actively queries GitHub's REST and GraphQL APIs using GitHub App installation tokens generated from the App's RSA private key.
- **Configurable Polling Frequency:** Workspace owners/admins can configure intervals between 1 and 15 minutes (default: 5 minutes) via **Settings → GitHub**.
- **Sync Now:** Admins can trigger an immediate full sync bypassing the interval backoff.
- **Safe Mode Handoffs:** Switching an existing workspace between webhook and polling modes performs a durable transition handoff to avoid missing events or duplicate task modifications.
