# GitHub integration

Orbit uses one GitHub App per workspace. Workspace owners and admins start App registration in workspace settings. GitHub returns the App credentials to Orbit. Orbit saves the App ID and encrypts the private key and webhook secret in its database. Each project then selects an installed repository and an issue label. No repository webhook and no Orbit API token are needed.

## GitHub host

`ORBIT_GITHUB_URL` selects one GitHub web origin for the entire installation, including GitHub sign-in and workspace Apps. It defaults to `https://github.com`. Set it before first startup to an HTTPS origin without credentials, a path, query or fragment; an optional port is allowed. Orbit detects the host and derives its endpoints:

| Web origin | REST API base | GraphQL endpoint |
|---|---|---|
| `https://github.com` | `https://api.github.com` | `https://api.github.com/graphql` |
| `https://acme.ghe.com` (GitHub Enterprise Cloud with data residency) | `https://api.acme.ghe.com` | `https://api.acme.ghe.com/graphql` |
| `https://github.example.com` (GitHub Enterprise Server) | `https://github.example.com/api/v3` | `https://github.example.com/api/graphql` |

For `ghe.com`, exactly one subdomain is required. Other hosts are treated as GitHub Enterprise Server; a non-default port is retained for its endpoints and for the Enterprise Cloud API. OAuth authorization and token URLs, App registration and installation links use the selected web origin. There is no separate API URL override.

The Orbit server must reach the chosen web and API hosts over HTTPS, and browsers must reach the web host. The GitHub host must reach Orbit's public HTTPS webhook and callback URLs. Configure DNS, firewalls and trusted TLS certificates accordingly; private Enterprise Server deployments need connectivity in both directions. GitHub Enterprise Server and Enterprise Cloud have not been tested against real services.

The database binds to one GitHub host. Existing databases with GitHub records are bound to `https://github.com`; a configured host that does not match prevents startup. Restore the matching setting if this happens. Changing hosts or migrating existing GitHub connections and identities to another host is not supported. Before using an alternate host, stop every old server sharing the database and upgrade it to a version that enforces the binding. Old binaries ignore the binding lock, so a rolling upgrade cannot protect against an old server writing GitHub data for another host.

## Server setup

1. Give Orbit a public HTTPS URL that serves both the UI and `/api/v1/integrations/github/webhook` to GitHub. In production, set the server's canonical `ORBIT_PUBLIC_ORIGIN=https://orbit.example.com`. For local testing, use a public HTTPS tunnel to the Orbit web server with its `/api` proxy; Orbit uses the HTTPS domain open in your browser when you start App registration.
2. Generate a 32-byte key with `openssl rand -base64 32`. Store it as `ORBIT_APP_KEY`, or put it in a file and set `ORBIT_APP_KEY_FILE` to the file path. Keep this key outside the database and its backups. Orbit cannot verify webhooks or use the saved App credentials if the key is lost.
3. Set `ORBIT_GITHUB_URL` if needed as described above, then restart Orbit. Do not set `GITHUB_CONNECTIONS` or the older `GITHUB_*` connection variables; Orbit no longer reads them.

For local development, `just dev` also loads a root `.env.local` file. Put `ORBIT_APP_KEY=<base64-key>` there. Keep the file private and out of Git. Open Orbit through a public HTTPS tunnel to the web server before you select **Register GitHub App**. You do not need to set `ORBIT_PUBLIC_ORIGIN` to the tunnel URL for this development flow. A plain `http://127.0.0.1` tab cannot register a GitHub App.

## Register the workspace App and connect a project

1. Open **Settings → GitHub** as a workspace owner or admin.
2. Enter a GitHub organization name if the organization should own the App, or leave it blank for a personal App. Select **Register GitHub App**. Register and approve it on the chosen GitHub host. Orbit sends a private App manifest to GitHub; only the App owner can install it. GitHub sends a one-time code to Orbit, and Orbit stores the App credentials. This needs a reachable HTTPS origin.
3. Select **Install or change repositories** in workspace settings. Install the App on the GitHub account and select the repositories. GitHub sends installation events to Orbit.
4. Open **Tasks → Project settings → GitHub** and refresh after installation. Select a repository and enter a GitHub label, then save. Create the same label in GitHub. You can connect more projects to the same repository if each project uses a different label. A project has one repository connection.
5. Add that label to an issue or pull request. Orbit creates one task in the selected project. Later GitHub edits update its title and body; close and reopen events update its status. A merged pull request completes its task. A pull request closed without a merge cancels its task. Reopening it moves the task to unstarted. Removing the label pauses updates, keeps the task, and shows a **GitHub sync paused** badge. Adding the label again resumes updates on the same task. Do not put two configured project labels on one issue or pull request.

If GitHub rejects the App manifest, fix the problem and select **Register GitHub App** again. A new attempt replaces the pending registration. GitHub sends [`installation` and `installation_repositories` events](https://docs.github.com/en/webhooks/webhook-events-and-payloads#installation) to all GitHub Apps automatically; these events must not appear in the manifest's `default_events` list.

The task title and description are read-only in Orbit. Other task fields remain editable. Comments do not sync. Orbit does not send task edits to GitHub.

A labeled pull request creates its own Orbit task, with its GitHub title, body, and pull-request link. Orbit does not search the pull request title, body, or branch for an Orbit task URL. An unlabeled pull request does not create a task. Removing its project label pauses sync, and adding the label again resumes the same task.

If you delete a linked task in Orbit, the next GitHub event with the project label restores that same task and applies the current GitHub title, body, and status. An event without the label keeps the task deleted and pauses sync.

GitHub App registration uses the [GitHub App manifest flow](https://docs.github.com/en/apps/sharing-github-apps/registering-a-github-app-from-a-manifest). GitHub sends signed events as described in its [webhook validation guide](https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries). App installation is described in [GitHub's installation guide](https://docs.github.com/en/apps/using-github-apps/installing-your-own-github-app).

## Planned: workspace polling mode

The setup above describes the current webhook integration. A future workspace-level polling mode is planned for Orbit instances that are accessible to users over private HTTPS but cannot receive inbound GitHub webhooks. The browser must be able to reach Orbit over HTTPS for registration and the callback; GitHub's acceptance of a particular private hostname is not guaranteed. This mode has not been verified against a real private instance.

In polling mode, the App manifest will leave webhooks inactive. Orbit will discover App installations and repository access through outbound GitHub API requests rather than installation events. Workspace owners and admins will be able to choose a polling interval from 1 to 15 minutes, with a default of 5 minutes. Shorter intervals reduce synchronization latency but consume more GitHub API quota; longer intervals conserve quota but delay updates.

Polling will remain one-way: Orbit will not write task changes back to GitHub. A successful, complete poll can pause sync for an item whose project label was removed, retaining its task so adding the label again can resume it. Failed or incomplete polls must not be treated as proof that a label was removed.

**Sync now** is planned to request a poll for the whole workspace, bypassing Orbit's own polling backoff. It will not reset or bypass GitHub's rate limits or their reset times.

If switching an existing workspace between webhooks and polling, review the GitHub App's webhook configuration as part of the switch: polling requires inactive webhooks, while webhook mode requires an active webhook and an endpoint reachable by GitHub.
