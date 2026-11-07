import { useMutation } from '@tanstack/react-query'
import type { GithubWorkspaceSettings } from '@orbit/api/generated/types.gen'
import * as WebBrowser from 'expo-web-browser'
import { type ReactNode, useState } from 'react'
import { Platform, View } from 'react-native'

import { SettingsCard, SettingsShell, WORKSPACE_NAV } from '../../../components/settings/Shell'
import { githubManifest } from '../../../data/workspace-settings-api'
import { saveGithubSyncSettings, syncGithubNow, type SyncMode } from '../../../data/github-sync-api'
import { useGithubSettings } from '../../../hooks/workspace-settings'
import { useOrbit } from '../../../orbit-context'
import { errorMessage } from '../../../session-context'
import { Button, Input, Text } from '../../../ui'

/** The workspace's GitHub App: register it, then install it on repositories (both on GitHub, in the browser). */
export default function GithubScreen() {
  const query = useGithubSettings()
  return (
    <SettingsShell title="Settings" nav={WORKSPACE_NAV} onRefresh={() => query.refetch()}>
      {query.isError ? (
        <SettingsCard title="GitHub">
          <Body>GitHub settings are not available.</Body>
        </SettingsCard>
      ) : query.data === undefined ? (
        <SettingsCard title="GitHub">
          <Body>Loading GitHub settings.</Body>
        </SettingsCard>
      ) : (
        <Github settings={query.data} refetch={query.refetch} />
      )}
    </SettingsShell>
  )
}

function Github({ settings, refetch }: { settings: GithubWorkspaceSettings; refetch: () => Promise<unknown> }) {
  const { client, session, workspaceId } = useOrbit()
  const [organization, setOrganization] = useState('')
  // GitHub's manifest flow takes a form POST. The app cannot post one into the system browser, so it opens the web
  // app's GitHub page there; the Expo web build posts the form itself, as the web does.
  const register = useMutation({
    mutationFn: async () => {
      if (Platform.OS !== 'web') {
        await WebBrowser.openBrowserAsync(`${session.server}/settings/github`)
        await refetch()
        return
      }
      const data = await githubManifest(client, workspaceId, organization.trim())
      const form = document.createElement('form')
      form.method = 'POST'
      form.action = data.action
      const input = document.createElement('input')
      input.type = 'hidden'
      input.name = 'manifest'
      input.value = JSON.stringify(data.manifest)
      form.append(input)
      document.body.append(form)
      form.submit()
    },
  })
  const install = async (url: string) => {
    await WebBrowser.openBrowserAsync(url)
    await refetch()
  }

  return (
    <SettingsCard title="GitHub" description="Manage the GitHub App for this workspace. Connect repositories to projects in each project's settings.">
      <SyncControls key={`${workspaceId}:${(settings as { sync_mode?: string }).sync_mode}:${(settings as { poll_interval_minutes?: number }).poll_interval_minutes}`} settings={settings} refetch={refetch} />
      {!settings.app_slug ? (
        <View className="items-start gap-2">
          <Body muted>Register a GitHub App from Orbit. GitHub will return you to this workspace.</Body>
          {settings.can_manage ? (
            <>
              {Platform.OS === 'web' ? (
                // the web's `Label` (leading-none) above the field, 6px apart
                <View className="self-stretch gap-1.5">
                  <Text className="font-noto-medium text-[14px] leading-[14px]">GitHub organization (optional)</Text>
                  <Input accessibilityLabel="GitHub organization (optional)" value={organization} onChangeText={setOrganization} placeholder="Leave blank for a personal App" autoCapitalize="none" autoCorrect={false} />
                </View>
              ) : null}
              {!settings.key_configured ? <Text className="text-[12px] leading-[16px] text-destructive">Orbit is not ready to register a GitHub App yet. Ask the Orbit administrator to set the app key.</Text> : null}
              <Button label="Register GitHub App" disabled={!settings.key_configured || register.isPending} onPress={() => register.mutate()} />
              {register.isError ? <Text className="text-[12px] leading-[16px] text-destructive">{errorMessage(register.error) || 'App registration could not start.'}</Text> : null}
            </>
          ) : (
            <Body muted>Ask a workspace owner or admin to register the App.</Body>
          )}
        </View>
      ) : (
        <View className="items-start gap-3">
          <Body>App: {settings.app_slug}</Body>
          {settings.can_manage && settings.install_url ? <Button label="Install or change repositories" variant="outline" onPress={() => void install(settings.install_url!)} /> : null}
          <View className="self-stretch">
            <Text className="mb-2 font-noto-medium text-[14px] leading-[20px]">Installed repositories</Text>
            {settings.repositories.length > 0 ? (
              <View accessibilityLabel="Installed repositories" className="gap-2">
                {settings.repositories.map((item) => (
                  <View key={`${item.installation_id}:${item.repository}`} className="rounded-md border border-border px-3 py-2">
                    <Body>{item.repository}</Body>
                  </View>
                ))}
              </View>
            ) : (
              <Body muted>No repositories installed yet.</Body>
            )}
          </View>
          <Body muted>After installation, open a project's settings to select a repository and issue label.</Body>
        </View>
      )}
    </SettingsCard>
  )
}

function SyncControls({ settings, refetch }: { settings: GithubWorkspaceSettings; refetch: () => Promise<unknown> }) {
  const { client, workspaceId } = useOrbit()
  const current = settings as GithubWorkspaceSettings & { sync_mode?: SyncMode; poll_interval_minutes?: number }
  const [mode, setMode] = useState<SyncMode>(current.sync_mode ?? 'webhooks')
  const [interval, setInterval] = useState(String(current.poll_interval_minutes ?? 5))
  const valid = /^\d+$/.test(interval) && Number(interval) >= 1 && Number(interval) <= 15
  const save = useMutation({ mutationFn: () => saveGithubSyncSettings(client, workspaceId, { sync_mode: mode, poll_interval_minutes: Number(interval) }), onSuccess: () => refetch() })
  const sync = useMutation({ mutationFn: () => syncGithubNow(client, workspaceId), onSuccess: () => refetch() })
  const available = current.sync_mode === 'webhooks' || current.sync_mode === 'polling'
  return <View accessibilityLabel="GitHub sync" className="mb-6 gap-3">
    <Text className="font-noto-medium text-[14px] leading-[20px]">GitHub sync</Text>
    {!available ? <Body muted>Sync settings are not available from this server.</Body> : <>
      <Text className="font-noto-medium text-[14px] leading-[20px]">Sync mode</Text>
      <View className="flex-row flex-wrap gap-2">
        <Button label={mode === 'webhooks' ? 'Webhooks (selected)' : 'Webhooks'} variant={mode === 'webhooks' ? 'primary' : 'outline'} disabled={!settings.can_manage || save.isPending} onPress={() => setMode('webhooks')} />
        <Button label={mode === 'polling' ? 'Polling (selected)' : 'Polling'} variant={mode === 'polling' ? 'primary' : 'outline'} disabled={!settings.can_manage || save.isPending} onPress={() => setMode('polling')} />
      </View>
      <Input label="Polling interval (1–15 minutes)" keyboardType="number-pad" value={interval} onChangeText={setInterval} editable={settings.can_manage && !save.isPending} />
      {!valid ? <Text className="text-[12px] leading-[16px] text-destructive">Enter a whole number from 1 to 15.</Text> : null}
      {settings.can_manage ? <View className="flex-row flex-wrap gap-2"><Button label="Save sync settings" disabled={!valid || save.isPending} onPress={() => save.mutate()} /><Button label="Sync now" variant="outline" disabled={sync.isPending} onPress={() => sync.mutate()} /></View> : <Body muted>Only workspace owners and admins can change settings or sync now.</Body>}
      {save.isSuccess ? <Body>Sync settings saved.</Body> : null}
      {save.isError ? <Text className="text-[12px] leading-[16px] text-destructive">{errorMessage(save.error) || 'Could not save sync settings.'}</Text> : null}
      {sync.isError ? <Text className="text-[12px] leading-[16px] text-destructive">{errorMessage(sync.error) || 'Sync could not start.'}</Text> : null}
      {sync.isSuccess ? <Body>Sync request completed.{sync.data == null ? '' : ` Result: ${JSON.stringify(sync.data)}`}</Body> : null}
    </>}
    <Body muted>Sync status and backoff are not available from this server.</Body>
  </View>
}

/** The web card body's `text-sm`. */
function Body({ children, muted = false }: { children: ReactNode; muted?: boolean }) {
  return <Text className={`text-[14px] leading-[20px] ${muted ? 'text-muted-foreground' : 'text-foreground'}`}>{children}</Text>
}
