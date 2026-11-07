import { afterEach, expect, test } from 'bun:test'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { WorkspaceContext } from '@/features/workspaces/workspaceContext'
import { GithubPage } from './GithubPage'
import { waitForAbsence } from '@/test/waitForAbsence'
import { testWorkspace } from '@/test/workspace'

const originalFetch = globalThis.fetch
const originalSubmit = HTMLFormElement.prototype.submit
afterEach(() => { globalThis.fetch = originalFetch; HTMLFormElement.prototype.submit = originalSubmit })

function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } })
  const workspace = testWorkspace()
  return render(<QueryClientProvider client={client}><WorkspaceContext.Provider value={{ workspace, workspaces: [workspace], selectWorkspace: () => {} }}><GithubPage /></WorkspaceContext.Provider></QueryClientProvider>)
}

test('workspace settings start GitHub App registration', async () => {
  let sentBody: unknown
  let requestPath = ''
  let submitted: { action: string; manifest: string | undefined } | undefined
  HTMLFormElement.prototype.submit = function () {
    submitted = { action: this.action, manifest: this.querySelector<HTMLInputElement>('input[name="manifest"]')?.value }
  }
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const request = input as Request
    if (request.method === 'POST') {
      requestPath = new URL(request.url).pathname
      sentBody = await request.json()
      return Response.json({ action: 'https://github.com/organizations/acme/settings/apps/new?state=secret', manifest: { name: 'Orbit', default_events: ['issues'] } })
    }
    return Response.json({ app_slug: null, install_url: null, can_manage: true, key_configured: true })
  }) as typeof fetch

  const view = renderPage()
  await view.findByRole('button', { name: 'Register GitHub App' })
  await userEvent.type(view.getByLabelText('GitHub organization (optional)'), 'acme')
  await userEvent.click(view.getByRole('button', { name: 'Register GitHub App' }))

  await waitFor(() => expect(submitted?.manifest).toContain('"default_events":["issues"]'))
  expect(submitted?.action).toContain('/organizations/acme/settings/apps/new')
  expect(requestPath).toBe('/api/v1/workspaces/workspace-1/github/manifest')
  expect(sentBody).toEqual({ organization: 'acme' })
})

test('workspace settings show installation controls after registration', async () => {
  globalThis.fetch = (async () => Response.json({ app_slug: 'orbit-test', install_url: 'https://github.com/apps/orbit-test/installations/new', repositories: [
    { installation_id: 1, repository: 'acme/first' },
    { installation_id: 2, repository: 'other/second' },
  ], can_manage: true, key_configured: true })) as unknown as typeof fetch
  const view = renderPage()
  expect(await view.findByText('App: orbit-test')).toBeTruthy()
  expect(view.getByRole('button', { name: 'Install or change repositories' })).toBeTruthy()
  expect([...view.getByRole('list', { name: 'Installed repositories' }).querySelectorAll('li')].map((item) => item.textContent)).toEqual(['acme/first', 'other/second'])
  expect(view.queryByRole('button', { name: 'Register GitHub App' })).toBeNull()
})

test('workspace settings show an empty state before repositories are installed', async () => {
  globalThis.fetch = (async () => Response.json({ app_slug: 'orbit-test', install_url: null, repositories: [], can_manage: false, key_configured: true })) as unknown as typeof fetch
  const view = renderPage()
  expect(await view.findByText('No repositories installed yet.')).toBeTruthy()
})

test('managers can save polling settings and run a sync', async () => {
  const requests: { method: string; path: string; body?: unknown }[] = []
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const request = input as Request
    const path = new URL(request.url).pathname
    requests.push({ method: request.method, path, ...(request.method === 'PUT' ? { body: await request.json() } : {}) })
    if (request.method === 'POST') return Response.json({ synced: 3 })
    return Response.json({ app_slug: null, install_url: null, repositories: [], can_manage: true, key_configured: true, sync_mode: 'webhooks', poll_interval_minutes: 5 })
  }) as typeof fetch
  const view = renderPage()
  await view.findByRole('radio', { name: 'Polling' })
  await userEvent.click(view.getByRole('radio', { name: 'Polling' }))
  const interval = view.getByLabelText('Polling interval (1–15 minutes)')
  await userEvent.clear(interval)
  await userEvent.type(interval, '15')
  await userEvent.click(view.getByRole('button', { name: 'Save sync settings' }))
  await waitFor(() => expect(requests.some((r) => r.method === 'PUT' && JSON.stringify(r.body) === JSON.stringify({ sync_mode: 'polling', poll_interval_minutes: 15 }))).toBe(true))
  await userEvent.click(view.getByRole('button', { name: 'Sync now' }))
  await view.findByText(/"synced":3/)
  expect(requests.some((r) => r.method === 'POST' && r.path === '/api/v1/workspaces/workspace-1/github/sync')).toBe(true)
})

test('members cannot change sync settings or run a sync', async () => {
  globalThis.fetch = (async () => Response.json({ app_slug: null, repositories: [], can_manage: false, key_configured: true, sync_mode: 'polling', poll_interval_minutes: 5 })) as unknown as typeof fetch
  const view = renderPage()
  const polling = await view.findByRole('radio', { name: 'Polling' })
  expect((polling as HTMLInputElement).disabled || polling.closest('fieldset')?.disabled).toBe(true)
  expect(view.queryByRole('button', { name: 'Sync now' })).toBeNull()
})

test('registration can be started again after browser Back restores the page', async () => {
  const submitted: string[] = []
  let requests = 0
  HTMLFormElement.prototype.submit = function () { submitted.push(this.action) }
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const request = input as Request
    if (request.method === 'POST') {
      requests += 1
      return Response.json({ action: `https://github.com/settings/apps/new?state=attempt-${requests}`, manifest: { default_events: ['issues', 'pull_request'] } })
    }
    return Response.json({ app_slug: null, install_url: null, can_manage: true, key_configured: true })
  }) as typeof fetch

  const view = renderPage()
  const register = await view.findByRole('button', { name: 'Register GitHub App' })
  await userEvent.click(register)
  await waitFor(() => expect(submitted).toHaveLength(1))
  await waitFor(() => expect(register.hasAttribute('disabled')).toBe(false))
  await userEvent.click(register)
  await waitFor(() => expect(submitted).toHaveLength(2))
  expect(submitted[0]).toContain('state=attempt-1')
  expect(submitted[1]).toContain('state=attempt-2')
})

test('registration can be retried after a pending-registration error', async () => {
  let requests = 0
  let submitted = false
  HTMLFormElement.prototype.submit = function () { submitted = true }
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const request = input as Request
    if (request.method === 'POST') {
      requests += 1
      if (requests === 1) return Response.json({ code: 'github_registration_pending', detail: 'Registration pending', status: 409, title: 'Conflict' }, { status: 409, headers: { 'content-type': 'application/problem+json' } })
      return Response.json({ action: 'https://github.com/settings/apps/new?state=retry', manifest: { default_events: ['issues', 'pull_request'] } })
    }
    return Response.json({ app_slug: null, install_url: null, can_manage: true, key_configured: true })
  }) as typeof fetch

  const view = renderPage()
  const register = await view.findByRole('button', { name: 'Register GitHub App' })
  await userEvent.click(register)
  await view.findByRole('alert')
  window.dispatchEvent(new Event('pageshow'))
  await waitForAbsence(() => view.queryByRole('alert'))
  await userEvent.click(register)
  await waitFor(() => expect(submitted).toBe(true))
  expect(requests).toBe(2)
  expect(view.queryByRole('alert')).toBeNull()
})
