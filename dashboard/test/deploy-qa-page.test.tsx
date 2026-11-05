import { afterEach, describe, expect, mock, test } from 'bun:test'
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { SWRConfig } from 'swr'
import type { ReactNode } from 'react'
import { AuthProvider } from '../src/lib/auth'
import { RouterProvider } from '../src/router'
import type {
  DeployQaConfigResponse,
  DeployQaSettings,
  DeployQaStatus,
  DeployQaTipStatus,
  DeployQaTipView,
} from '../src/lib/api'

type Role = 'admin' | 'viewer'

interface Call {
  url: string
  method: string
  body?: unknown
}

interface FetchResponse {
  ok: boolean
  status: number
  json: () => Promise<unknown>
}

const statuses: DeployQaTipStatus[] = ['pending', 'running', 'verified', 'unverified', 'failed', 'errored']

function tip(id: number, status: DeployQaTipStatus): DeployQaTipView {
  return {
    id,
    track: 'cloud',
    repo: 'appwrite/cloud',
    tag: `1.0.${id}`,
    status,
    published_at: '2026-11-01T00:00:00Z',
    release_url: `https://github.com/appwrite/cloud/releases/tag/1.0.${id}`,
    author_login: 'releaser',
    attempt_id: id + 100,
    discord_message_url: 'https://discord.com/channels/1/2/3',
    discord_thread_url: 'https://discord.com/channels/1/4',
    created_at: '2026-11-01T00:00:00Z',
    updated_at: '2026-11-01T00:00:00Z',
  }
}

const tips = statuses.map((status, index) => tip(index + 1, status))

const status: DeployQaStatus = {
  enabled: true,
  restart_required: false,
  poll_interval_ms: 300000,
  timeout_secs: 1800,
  skip_if_previous_running: true,
  discord_channel_id: '42',
  tracks: [
    { name: 'cloud', repo: 'appwrite/cloud', tag_filter: 'any', last_seen: tips[2], in_flight: true },
    { name: 'edge', repo: 'appwrite/edge', tag_filter: 'any', last_seen: null, in_flight: false },
  ],
}

const RESTART_BANNER =
  /Restart the claudear process serving this console \(and the daemon, if it runs separately\) to apply them/

function configResponse(overrides: Partial<DeployQaConfigResponse> = {}): DeployQaConfigResponse {
  return {
    settings: {
      enabled: true,
      poll_interval_ms: 300000,
      skip_if_previous_running: true,
      timeout_secs: 1800,
      discord_channel_id: '42',
      github_discord_map_path: null,
      instructions_path: null,
      tracks: [
        { name: 'cloud', repo: 'appwrite/cloud', tag_filter: 'any' },
        { name: 'edge', repo: 'appwrite/edge', tag_filter: 'any' },
      ],
    },
    tag_filter_kinds: [
      { kind: 'any', requires_value: false },
      { kind: 'suffix', requires_value: true },
      { kind: 'not_suffix', requires_value: true },
    ],
    track_name_separator: ':',
    restart_required: false,
    writable: true,
    revision: 'revision-1',
    ...overrides,
  }
}

function json(statusCode: number, body: unknown): Promise<FetchResponse> {
  return Promise.resolve({ ok: statusCode >= 200 && statusCode < 300, status: statusCode, json: async () => body })
}

function createHarness({
  role,
  statusBody = status,
  config = configResponse(),
}: {
  role: Role
  statusBody?: DeployQaStatus
  config?: DeployQaConfigResponse
}) {
  const calls: Call[] = []
  let putResponse: () => Promise<FetchResponse> = () =>
    json(200, { ok: true, restart_required: true, revision: 'revision-2', message: 'Saved to claudear.toml.' })

  globalThis.fetch = mock((input: string | URL | Request, init?: RequestInit) => {
    const url = typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url
    const method = init?.method ?? 'GET'
    const body = typeof init?.body === 'string' ? JSON.parse(init.body) : undefined
    calls.push({ url, method, body })

    if (url.endsWith('/api/auth/me')) {
      return json(200, { id: 1, email: `${role}@test.com`, name: role, role, avatar_url: null })
    }
    if (url.includes('/api/deploy-qa/config')) {
      return method === 'PUT' ? putResponse() : json(200, config)
    }
    if (url.includes('/api/deploy-qa/tips')) {
      return json(200, { tips })
    }
    if (url.endsWith('/api/deploy-qa')) {
      return json(200, statusBody)
    }
    return json(200, {})
  }) as unknown as typeof fetch

  return {
    calls,
    respondToPut(next: () => Promise<FetchResponse>) {
      putResponse = next
    },
    puts() {
      return calls.filter(call => call.method === 'PUT' && call.url.includes('/api/deploy-qa/config'))
    },
  }
}

function Wrapper({ children }: { children: ReactNode }) {
  return (
    <SWRConfig value={{ provider: () => new Map(), dedupingInterval: 0 }}>
      <RouterProvider>
        <AuthProvider>{children}</AuthProvider>
      </RouterProvider>
    </SWRConfig>
  )
}

async function renderPage() {
  const DeployQaPage = (await import('../src/pages/deploy-qa')).default
  render(<DeployQaPage />, { wrapper: Wrapper })
}

async function renderAdminSettings() {
  await renderPage()
  await waitFor(() => {
    expect(screen.getByLabelText('Track 2 name')).toBeTruthy()
  })
}

describe('DeployQaPage', () => {
  const originalFetch = globalThis.fetch

  afterEach(() => {
    cleanup()
    globalThis.fetch = originalFetch
    window.history.replaceState(null, '', '/')
  })

  test('viewer sees status and runs but never requests the admin config', async () => {
    const harness = createHarness({ role: 'viewer' })
    await renderPage()

    await waitFor(() => {
      expect(screen.getByText('1.0.6')).toBeTruthy()
    })
    expect(screen.getByRole('heading', { name: 'Status' })).toBeTruthy()
    expect(screen.getByRole('heading', { name: 'Tracks' })).toBeTruthy()
    expect(screen.queryByRole('heading', { name: 'Settings' })).toBeNull()
    expect(harness.calls.some(call => call.url.includes('/api/deploy-qa/config'))).toBe(false)
  })

  test('renders every tip status badge, links, and the in-flight marker', async () => {
    createHarness({ role: 'viewer' })
    await renderPage()

    await waitFor(() => {
      expect(screen.getByText('1.0.6')).toBeTruthy()
    })
    for (const tipStatus of statuses) {
      expect(screen.getAllByText(tipStatus).length).toBeGreaterThanOrEqual(1)
    }
    expect(screen.getAllByText('In flight')).toHaveLength(2)
    expect(screen.getByText('appwrite/cloud').getAttribute('href')).toBe('https://github.com/appwrite/cloud/releases')
    expect(screen.getAllByText('Release')[0].getAttribute('href')).toBe(tips[0].release_url)
    expect(screen.getAllByText('Discord thread')[0].getAttribute('href')).toBe('https://discord.com/channels/1/4')

    fireEvent.click(screen.getAllByText('Run log')[0])
    expect(window.location.href).toContain('/attempts?id=101')
  })

  test('the track filter requests tips for that track', async () => {
    const harness = createHarness({ role: 'viewer' })
    await renderPage()

    await waitFor(() => {
      expect(screen.getByText('1.0.6')).toBeTruthy()
    })
    fireEvent.change(screen.getByLabelText('Filter runs by track'), { target: { value: 'edge' } })

    await waitFor(() => {
      expect(harness.calls.some(call => call.url.endsWith('/api/deploy-qa/tips?track=edge'))).toBe(true)
    })
  })

  test('the restart banner renders when the running config is stale', async () => {
    createHarness({ role: 'viewer', statusBody: { ...status, restart_required: true } })
    await renderPage()

    await waitFor(() => {
      expect(screen.getByText(RESTART_BANNER)).toBeTruthy()
    })
  })

  test('admin save sends the composed tag filter and the revision', async () => {
    const harness = createHarness({ role: 'admin' })
    await renderAdminSettings()

    expect(
      screen.getByText(
        'Settings are saved to claudear.toml and take effect after claudear restarts. Saving rewrites the file in normalised form (comments removed).',
      ),
    ).toBeTruthy()

    fireEvent.change(screen.getByLabelText('Track 2 tag filter kind'), { target: { value: 'suffix' } })
    fireEvent.change(screen.getByLabelText('Track 2 tag filter value'), { target: { value: '-db' } })
    fireEvent.click(screen.getByRole('button', { name: 'Save' }))

    await waitFor(() => {
      expect(harness.puts()).toHaveLength(1)
    })
    const body = harness.puts()[0].body as { settings: { tracks: { tag_filter: string }[] }; revision: string }
    expect(body.settings.tracks[1].tag_filter).toBe('suffix:-db')
    expect(body.settings.tracks[0].tag_filter).toBe('any')
    expect(body.revision).toBe('revision-1')

    await waitFor(() => {
      expect(screen.getByText('Saved to claudear.toml.')).toBeTruthy()
      expect(screen.getByText(RESTART_BANNER)).toBeTruthy()
    })
  })

  test('a tag filter kind the frontend has never seen works when the API lists it', async () => {
    const base = configResponse()
    const harness = createHarness({
      role: 'admin',
      config: configResponse({
        tag_filter_kinds: [...base.tag_filter_kinds, { kind: 'prefix', requires_value: true }],
        settings: {
          ...base.settings,
          tracks: [
            { name: 'cloud', repo: 'appwrite/cloud', tag_filter: 'prefix:v1.' },
            { name: 'edge', repo: 'appwrite/edge', tag_filter: 'any' },
          ],
        },
      }),
    })
    await renderAdminSettings()

    expect((screen.getByLabelText('Track 1 tag filter kind') as HTMLSelectElement).value).toBe('prefix')
    expect((screen.getByLabelText('Track 1 tag filter value') as HTMLInputElement).value).toBe('v1.')

    fireEvent.change(screen.getByLabelText('Track 2 tag filter kind'), { target: { value: 'prefix' } })
    fireEvent.change(screen.getByLabelText('Track 2 tag filter value'), { target: { value: 'v2.' } })
    fireEvent.click(screen.getByRole('button', { name: 'Save' }))

    await waitFor(() => {
      expect(harness.puts()).toHaveLength(1)
    })
    const body = harness.puts()[0].body as { settings: { tracks: { tag_filter: string }[] } }
    expect(body.settings.tracks.map(track => track.tag_filter)).toEqual(['prefix:v1.', 'prefix:v2.'])
  })

  test('a duplicate track name blocks the save and shows the message', async () => {
    const harness = createHarness({ role: 'admin' })
    await renderAdminSettings()

    fireEvent.change(screen.getByLabelText('Track 2 name'), { target: { value: 'cloud' } })

    await waitFor(() => {
      expect(screen.getByText("Track name 'cloud' is used by more than one track.")).toBeTruthy()
    })
    const save = screen.getByRole('button', { name: 'Save' }) as HTMLButtonElement
    expect(save.disabled).toBe(true)
    fireEvent.submit(save.closest('form') as HTMLFormElement)
    await new Promise(resolve => setTimeout(resolve, 20))
    expect(harness.puts()).toHaveLength(0)
  })

  test('a whitespace-padded track name blocks the save instead of renaming the track', async () => {
    const harness = createHarness({
      role: 'admin',
      config: configResponse({
        settings: {
          ...configResponse().settings,
          tracks: [
            { name: ' cloud', repo: 'appwrite/cloud', tag_filter: 'any' },
            { name: 'edge', repo: 'appwrite/edge', tag_filter: 'any' },
          ],
        },
      }),
    })
    await renderAdminSettings()

    expect((screen.getByLabelText('Track 1 name') as HTMLInputElement).value).toBe(' cloud')
    expect(screen.getByText('Track name must not start or end with whitespace.')).toBeTruthy()
    const save = screen.getByRole('button', { name: 'Save' }) as HTMLButtonElement
    expect(save.disabled).toBe(true)
    fireEvent.submit(save.closest('form') as HTMLFormElement)
    await new Promise(resolve => setTimeout(resolve, 20))
    expect(harness.puts()).toHaveLength(0)
  })

  test('save sends track names unchanged while trimming repos and blank paths', async () => {
    const harness = createHarness({ role: 'admin' })
    await renderAdminSettings()

    fireEvent.change(screen.getByLabelText('Track 2 name'), { target: { value: 'edge-db' } })
    fireEvent.change(screen.getByLabelText('Track 2 repository'), { target: { value: '  appwrite/edge  ' } })
    fireEvent.change(screen.getByLabelText('GitHub to Discord map path'), { target: { value: '   ' } })
    fireEvent.change(screen.getByLabelText('Instructions path'), { target: { value: ' /etc/qa.md ' } })
    fireEvent.click(screen.getByRole('button', { name: 'Save' }))

    await waitFor(() => {
      expect(harness.puts()).toHaveLength(1)
    })
    const body = harness.puts()[0].body as { settings: DeployQaSettings }
    expect(body.settings.tracks).toEqual([
      { name: 'cloud', repo: 'appwrite/cloud', tag_filter: 'any' },
      { name: 'edge-db', repo: 'appwrite/edge', tag_filter: 'any' },
    ])
    expect(body.settings.github_discord_map_path).toBeNull()
    expect(body.settings.instructions_path).toBe('/etc/qa.md')
  })

  test('adding and removing tracks edits the list sent to the server', async () => {
    const harness = createHarness({ role: 'admin' })
    await renderAdminSettings()

    fireEvent.click(screen.getByRole('button', { name: 'Add track' }))
    fireEvent.change(screen.getByLabelText('Track 3 name'), { target: { value: 'console' } })
    fireEvent.change(screen.getByLabelText('Track 3 repository'), { target: { value: 'appwrite/console' } })
    fireEvent.click(screen.getByRole('button', { name: 'Remove track 1' }))
    fireEvent.click(screen.getByRole('button', { name: 'Save' }))

    await waitFor(() => {
      expect(harness.puts()).toHaveLength(1)
    })
    const body = harness.puts()[0].body as { settings: DeployQaSettings }
    expect(body.settings.tracks).toEqual([
      { name: 'edge', repo: 'appwrite/edge', tag_filter: 'any' },
      { name: 'console', repo: 'appwrite/console', tag_filter: 'any' },
    ])
  })

  test("a 400 shows the server's error message", async () => {
    const harness = createHarness({ role: 'admin' })
    harness.respondToPut(() => json(400, { error: 'X', code: 'invalid' }))
    await renderAdminSettings()

    fireEvent.click(screen.getByRole('button', { name: 'Save' }))

    await waitFor(() => {
      expect(within(screen.getByRole('alert')).getByText('X')).toBeTruthy()
    })
    expect(screen.queryByRole('button', { name: 'Reload' })).toBeNull()
  })

  test('a revision mismatch offers a reload', async () => {
    const harness = createHarness({ role: 'admin' })
    harness.respondToPut(() => json(409, { error: 'claudear.toml changed on disk', code: 'revision_mismatch' }))
    await renderAdminSettings()

    fireEvent.click(screen.getByRole('button', { name: 'Save' }))

    await waitFor(() => {
      expect(screen.getByText('claudear.toml changed on disk')).toBeTruthy()
    })
    const configGets = () =>
      harness.calls.filter(call => call.method === 'GET' && call.url.includes('/api/deploy-qa/config')).length
    const before = configGets()
    fireEvent.click(screen.getByRole('button', { name: 'Reload' }))
    await waitFor(() => {
      expect(configGets()).toBe(before + 1)
    })
  })

  test('a read-only config disables the settings with an explanation', async () => {
    createHarness({ role: 'admin', config: configResponse({ writable: false }) })
    await renderAdminSettings()

    expect(screen.getByText(/claudear.toml is read-only for this process/)).toBeTruthy()
    expect((screen.getByLabelText('Track 1 name') as HTMLInputElement).closest('fieldset')?.disabled).toBe(true)
  })
})
