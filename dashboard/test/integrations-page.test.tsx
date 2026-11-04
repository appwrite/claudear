import { afterEach, beforeEach, describe, expect, mock, test } from 'bun:test'
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { SWRConfig } from 'swr'
import type { ReactNode } from 'react'
import { AuthProvider, useAuth } from '../src/lib/auth'
import { RouterProvider } from '../src/router'
import type { FeatureCategory, FeatureStatus } from '../src/lib/api'

type Role = 'admin' | 'viewer'

const leakedSecret = 'sk-live-LEAKED-SECRET-VALUE'

function feature(overrides: Partial<FeatureStatus> & Pick<FeatureStatus, 'key' | 'label' | 'category'>): FeatureStatus {
  return {
    description: `${overrides.label} description`,
    configured: true,
    enabled: true,
    settings: [],
    secrets: [],
    ...overrides,
  }
}

const features: FeatureStatus[] = [
  feature({ key: 'mcp_server', label: 'MCP server', category: 'platform' }),
  feature({
    key: 'issues.linear',
    label: 'Linear',
    category: 'source',
    settings: [
      { name: 'trigger_labels', value: ['claudear', 'bug'] },
      { name: 'poll_interval_ms', value: 300000 },
      { name: 'team_id', value: null },
    ],
    secrets: [
      { name: 'api_key', set: true, value: leakedSecret } as FeatureStatus['secrets'][number],
      { name: 'webhook_secret', set: false },
    ],
  }),
  feature({ key: 'issues.sentry', label: 'Sentry', category: 'source', enabled: false }),
  feature({ key: 'issues.jira', label: 'Jira', category: 'source', configured: false, enabled: false }),
  feature({ key: 'scm.github', label: 'GitHub', category: 'scm' }),
  feature({ key: 'notifiers.discord', label: 'Discord notifier', category: 'notifier', configured: false, enabled: false }),
  feature({
    key: 'deploy_qa',
    label: 'Deploy QA',
    category: 'automation',
    settings: [
      { name: 'skip_if_previous_running', value: true },
      { name: 'filters', value: { cloud: 'any' } },
    ],
  }),
  feature({ key: 'llm', label: 'Local LLM', category: 'intelligence', enabled: false }),
]

const expectedCategoryOrder: Array<[FeatureCategory, string]> = [
  ['source', 'Issue sources'],
  ['scm', 'Source control'],
  ['notifier', 'Notifiers'],
  ['automation', 'Automation'],
  ['intelligence', 'Intelligence'],
  ['platform', 'Platform'],
]

function json(status: number, body: unknown) {
  return Promise.resolve({ ok: status >= 200 && status < 300, status, json: async () => body })
}

function createHarness({ role, featuresResponse = () => json(200, { features }) }: {
  role: Role
  featuresResponse?: () => ReturnType<typeof json>
}) {
  const urls: string[] = []
  globalThis.fetch = mock((input: string | URL | Request) => {
    const url = typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url
    urls.push(url)
    if (url.endsWith('/api/auth/me')) {
      return json(200, { id: 1, email: `${role}@test.com`, name: role, role, avatar_url: null })
    }
    if (url.endsWith('/api/features')) {
      return featuresResponse()
    }
    return json(200, {})
  }) as unknown as typeof fetch
  return { urls }
}

function SignedInRole() {
  const { user } = useAuth()
  return <output aria-label="Signed in role">{user?.role ?? ''}</output>
}

function Wrapper({ children }: { children: ReactNode }) {
  return (
    <SWRConfig value={{ provider: () => new Map(), dedupingInterval: 0 }}>
      <RouterProvider>
        <AuthProvider>
          <SignedInRole />
          {children}
        </AuthProvider>
      </RouterProvider>
    </SWRConfig>
  )
}

async function renderPage() {
  const IntegrationsPage = (await import('../src/pages/integrations')).default
  render(<IntegrationsPage />, { wrapper: Wrapper })
  await waitFor(() => {
    expect(screen.getByRole('article', { name: 'Linear' })).toBeTruthy()
  })
}

function card(label: string) {
  return screen.getByRole('article', { name: label })
}

describe('IntegrationsPage', () => {
  const originalFetch = globalThis.fetch

  beforeEach(() => {
    window.history.pushState(null, '', '/integrations')
  })

  afterEach(() => {
    cleanup()
    globalThis.fetch = originalFetch
  })

  test('requests the features endpoint and shows the restart note', async () => {
    const harness = createHarness({ role: 'viewer' })
    await renderPage()

    expect(harness.urls.some(url => url.endsWith('/api/features'))).toBe(true)
    expect(
      screen.getByText('Shows the configuration claudear is running with. Changes take effect after restart.'),
    ).toBeTruthy()
  })

  test('groups features by category in the declared order', async () => {
    createHarness({ role: 'viewer' })
    await renderPage()

    const headings = screen.getAllByRole('heading', { level: 3 }).filter(heading => heading.id.startsWith('category-'))
    expect(headings.map(heading => heading.textContent)).toEqual(expectedCategoryOrder.map(([, label]) => label))

    const sources = screen.getByRole('region', { name: 'Issue sources' })
    expect(within(sources).getAllByRole('article').map(article => article.getAttribute('aria-labelledby'))).toEqual([
      'feature-issues.linear',
      'feature-issues.sentry',
      'feature-issues.jira',
    ])
    expect(within(sources).getByText(/1 of 3 enabled/)).toBeTruthy()
    expect(within(screen.getByRole('region', { name: 'Platform' })).getByRole('article', { name: 'MCP server' })).toBeTruthy()
    expect(within(screen.getByRole('region', { name: 'Source control' })).getByRole('article', { name: 'GitHub' })).toBeTruthy()
  })

  test('places a category the page does not know under Other', async () => {
    createHarness({
      role: 'viewer',
      featuresResponse: () =>
        json(200, {
          features: [
            ...features,
            feature({ key: 'future', label: 'Future thing', category: 'telepathy' as FeatureCategory }),
          ],
        }),
    })
    await renderPage()

    expect(within(screen.getByRole('region', { name: 'Other' })).getByRole('article', { name: 'Future thing' })).toBeTruthy()
  })

  test('renders the three badge states', async () => {
    createHarness({ role: 'viewer' })
    await renderPage()

    expect(within(card('Linear')).getByText('Enabled')).toBeTruthy()
    expect(within(card('Sentry')).getByText('Disabled')).toBeTruthy()
    expect(within(card('Jira')).getByText('Not configured')).toBeTruthy()
    expect(within(card('Discord notifier')).getByText('Not configured')).toBeTruthy()
    expect(within(card('Local LLM')).getByText('Disabled')).toBeTruthy()
  })

  test('renders settings and secret presence without any secret value', async () => {
    createHarness({ role: 'viewer' })
    await renderPage()

    const linear = card('Linear')
    expect(within(linear).getByText('claudear, bug')).toBeTruthy()
    expect(within(linear).getByText('300000')).toBeTruthy()
    expect(within(linear).getByText('--')).toBeTruthy()
    expect(within(card('Deploy QA')).getByText('Yes')).toBeTruthy()
    expect(within(card('Deploy QA')).getByText('{"cloud":"any"}')).toBeTruthy()

    const chips = within(within(linear).getByRole('list', { name: 'Secrets' })).getAllByRole('listitem')
    expect(chips.map(chip => chip.textContent)).toEqual(['api_keyset', 'webhook_secretnot set'])

    expect(document.body.innerHTML).not.toContain(leakedSecret)
    expect(document.body.textContent).not.toContain(leakedSecret)
  })

  test('admins see Edit in Config on every card and it opens the config page', async () => {
    createHarness({ role: 'admin' })
    await renderPage()

    await waitFor(() => {
      expect(screen.getAllByRole('button', { name: 'Edit in Config' })).toHaveLength(features.length)
    })
    fireEvent.click(within(card('Linear')).getByRole('button', { name: 'Edit in Config' }))
    expect(window.location.pathname).toBe('/config')
  })

  test('viewers never see Edit in Config', async () => {
    createHarness({ role: 'viewer' })
    await renderPage()

    await waitFor(() => {
      expect(screen.getByLabelText('Signed in role').textContent).toBe('viewer')
    })

    expect(screen.queryByRole('button', { name: 'Edit in Config' })).toBeNull()
  })

  test('links features to their own pages', async () => {
    createHarness({ role: 'viewer' })
    await renderPage()

    expect(within(card('Linear')).queryByRole('button', { name: /^Open / })).toBeNull()
    fireEvent.click(within(card('Deploy QA')).getByRole('button', { name: 'Open Deploy QA' }))
    expect(window.location.pathname).toBe('/deploy-qa')
    fireEvent.click(within(card('Local LLM')).getByRole('button', { name: 'Open Models' }))
    expect(window.location.pathname).toBe('/models')
  })

  test('shows the server error message when loading fails', async () => {
    createHarness({ role: 'viewer', featuresResponse: () => json(500, { error: 'features unavailable' }) })
    const IntegrationsPage = (await import('../src/pages/integrations')).default
    render(<IntegrationsPage />, { wrapper: Wrapper })

    await waitFor(() => {
      expect(screen.getByRole('alert').textContent).toBe('features unavailable')
    })
  })
})
