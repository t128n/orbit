# Configure GitHub Enterprise hosts and polling sync

Orbit-CX extends the upstream Orbit GitHub App integration in two ways: you can point an installation at any GitHub host, and a workspace can sync by polling instead of by inbound webhook. This page covers both, plus the connectivity and host-binding rules you need before you deploy either one.

## Configure a custom GitHub host

`ORBIT_GITHUB_URL` selects one GitHub web origin for the whole installation: sign-in, OAuth callbacks, and workspace Apps. Upstream Orbit hardcodes `https://github.com`.

Set the variable before the first startup. Orbit derives the API endpoints from the origin:

| Web origin | Host type | REST API base | GraphQL endpoint |
|---|---|---|---|
| `https://github.com` | GitHub.com | `https://api.github.com` | `https://api.github.com/graphql` |
| `https://acme.ghe.com` | Enterprise Cloud with data residency | `https://api.acme.ghe.com` | `https://api.acme.ghe.com/graphql` |
| `https://github.example.com` | Enterprise Server | `https://github.example.com/api/v3` | `https://github.example.com/api/graphql` |

Orbit applies these detection rules:

- A `ghe.com` origin needs exactly one subdomain, as in `acme.ghe.com`.
- Orbit treats every other non-default origin as Enterprise Server.
- Orbit keeps a non-default port in the derived endpoints.
- GitHub App registration and installation links, plus the OAuth authorize and token URLs, use the selected web origin. There is no separate API URL override.

### Accepted origin format

Set `ORBIT_GITHUB_URL` to an HTTPS origin with no credentials, path, query, or fragment. An optional port is allowed. Orbit normalizes the value and refuses to start on anything else.

### Network requirements

Connectivity has to work in both directions:

- The Orbit server reaches the GitHub web and API hosts over HTTPS.
- Browsers reach the GitHub web host for sign-in and App approval.
- The GitHub host reaches Orbit: its callback URL in both modes, and `/api/v1/integrations/github/webhook` in webhook mode.

Configure name resolution, firewall rules, and trusted certificates for both paths. Private Enterprise Server deployments still need reachability from GitHub back to Orbit unless you use polling mode.

GitHub Enterprise Server and Enterprise Cloud have not been tested against real services in this fork.

## Bind the database to one host

A database stores GitHub records for exactly one host, and Orbit enforces that binding at startup:

1. On first use of the GitHub integration, Orbit writes the normalized origin to the `github_host_binding` table.
2. Every later startup compares `ORBIT_GITHUB_URL` with that row.
3. On a mismatch, startup stops with an error instead of writing mixed-host records.

Orbit keeps existing databases that already hold GitHub records bound to `https://github.com`. Restore the matching value to start the server again.

Changing hosts, or migrating existing GitHub connections and identities to another host, is unsupported. Before you switch to a non-default host, stop every server that shares the database and upgrade it to a build with this patch. Older binaries ignore the binding lock, so a rolling upgrade cannot stop an old server from writing GitHub data for a different host. See [Operate an Orbit-CX instance](operations.md).

## Synchronize a workspace by polling

A workspace can sync with GitHub through outbound API calls only. This mode suits instances that users reach over private HTTPS but that GitHub cannot reach over the public internet, such as deployments behind a corporate firewall, a virtual private network (VPN), or a private subnet.

In polling mode:

- The GitHub App manifest leaves out webhook attributes and events, so GitHub does not validate that Orbit is reachable.
- Orbit queries the GitHub REST and GraphQL APIs with installation tokens, which it generates from the App’s RSA private key.
- Installations and repository access come from those API calls instead of installation webhook events.

### Choose a mode and interval

Workspace owners and admins set the mode in **Settings → GitHub**:

1. Select the sync mode: webhooks (the default) or polling.
2. For polling, set an interval between 1 and 15 minutes. The default is 5 minutes.
3. Select **Save sync settings**.

A shorter interval lowers sync latency but uses more GitHub API quota. A longer interval uses less quota and delays updates.

### Request an immediate poll

**Sync now** polls the whole workspace at once and bypasses Orbit’s own backoff. It does not reset or bypass GitHub rate limits or their reset times.

## Switch a workspace between modes

Orbit fences writes and moves the workspace through a durable handoff, so a switch does not lose events or apply the same task change twice. The two directions differ:

- Polling to webhooks: Orbit runs one complete poll before it starts handling webhook deliveries. Deliveries that arrive during the handoff return `503`, which makes GitHub retry them.
- Webhooks to polling: Orbit applies the new mode and records a pending transition. Only a poll that starts after the request can complete it, so a poll already in flight cannot half-apply the switch.

## Limits of polling sync

Polling is one-way. Orbit reads issues and pull requests from GitHub and never writes task changes back.

A complete, successful poll can pause sync for an item whose project label disappeared, and Orbit keeps the task so that adding the label again resumes sync. A failed or partial poll is never treated as proof that a label was removed.
