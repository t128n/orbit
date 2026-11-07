import type { OrbitClient } from './orbit'

export type SyncMode = 'webhooks' | 'polling'
export type SyncSettings = { sync_mode: SyncMode; poll_interval_minutes: number }

export async function saveGithubSyncSettings(client: OrbitClient, workspaceId: string, body: SyncSettings) {
  await client.put({ url: '/api/v1/workspaces/{workspace_id}/github', path: { workspace_id: workspaceId }, body, headers: { 'Content-Type': 'application/json' }, throwOnError: true })
}

// The sync response contract is not yet published; retain its contents without interpreting it.
export async function syncGithubNow(client: OrbitClient, workspaceId: string): Promise<unknown> {
  const { data } = await client.post({ url: '/api/v1/workspaces/{workspace_id}/github/sync', path: { workspace_id: workspaceId }, throwOnError: true })
  return data
}
