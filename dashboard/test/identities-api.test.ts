import { afterEach, beforeEach, describe, expect, mock, test } from 'bun:test'
import {
  ApiError,
  SENSITIVE_IDENTITY_FIELDS,
  acceptInvite,
  createUser,
  deleteUser,
  fetchIdentities,
  fetchUserIdentities,
  inspectInvite,
  inviteAbsoluteUrl,
  issueInvite,
  resetUserIdentities,
  revokeInvite,
  setOnUnauthorized,
  updateUser,
  updateUserIdentities,
  type Identities,
  type IdentityViolation,
  type UserIdentities,
  type UserInvite,
} from '../src/lib/api'

const CSRF_TOKEN = 'csrf-test-token'

const emptyIdentities: Identities = {
  linear_names: [],
  github_usernames: [],
  sentry_usernames: [],
  jira_usernames: [],
  gitlab_usernames: [],
  discord_id: null,
  slack_id: null,
  email: null,
  push_user_key: null,
  sms_number: null,
  whatsapp_number: null,
  telegram_chat_id: null,
}

const jake: UserIdentities = {
  user_id: 3,
  name: 'Jake',
  login_email: 'jake@example.com',
  person_key: 'jake',
  slug: 'jake',
  origin: 'console',
  status: 'active',
  redacted: false,
  identities: { ...emptyIdentities, github_usernames: ['jakebarnby'] },
  updated_at: '2026-11-04 10:00:00',
}

const invite: UserInvite = {
  invite_link: '/invite#cldi_abc',
  invite_url: null,
  expires_at: '2026-11-11 10:00:00',
}

type FetchMock = ReturnType<typeof mock<(url: string, init?: RequestInit) => Promise<Response>>>

function mockFetch(data: unknown, status = 200): FetchMock {
  const fetchMock = mock((_url: string, _init?: RequestInit) =>
    Promise.resolve({
      ok: status >= 200 && status < 300,
      status,
      json: () => Promise.resolve(data),
    } as Response),
  )
  globalThis.fetch = fetchMock as unknown as typeof fetch
  return fetchMock
}

function lastCall(fetchMock: FetchMock): { url: string; init: RequestInit } {
  const [url, init] = fetchMock.mock.calls[fetchMock.mock.calls.length - 1]
  return { url, init: init ?? {} }
}

function header(init: RequestInit, name: string): string | undefined {
  return (init.headers as Record<string, string> | undefined)?.[name]
}

async function captureError(promise: Promise<unknown>): Promise<ApiError> {
  try {
    await promise
  } catch (error) {
    if (error instanceof ApiError) return error
    throw error
  }
  throw new Error('expected the request to reject')
}

describe('identities api', () => {
  const originalFetch = globalThis.fetch

  beforeEach(() => {
    document.cookie = `claudear_csrf=${CSRF_TOKEN}`
  })

  afterEach(() => {
    globalThis.fetch = originalFetch
    setOnUnauthorized(() => {})
    document.cookie = 'claudear_csrf=; expires=Thu, 01 Jan 1970 00:00:00 GMT'
  })

  test('fetchIdentities gets the directory', async () => {
    const fetchMock = mockFetch([jake])
    const result = await fetchIdentities()
    const { url, init } = lastCall(fetchMock)
    expect(url).toBe('/api/identities')
    expect(init.method).toBe('GET')
    expect(result).toEqual([jake])
  })

  test('fetchUserIdentities gets one user', async () => {
    const fetchMock = mockFetch(jake)
    const result = await fetchUserIdentities(3)
    const { url, init } = lastCall(fetchMock)
    expect(url).toBe('/api/users/3/identities')
    expect(init.method).toBe('GET')
    expect(result.slug).toBe('jake')
    expect(result.person_key).toBe('jake')
    expect(result.login_email).toBe('jake@example.com')
  })

  test('updateUserIdentities puts the sparse update with the CSRF header', async () => {
    const fetchMock = mockFetch(jake)
    const result = await updateUserIdentities(3, {
      slug: 'jake',
      github_usernames: ['jakebarnby'],
      sms_number: '',
    })
    const { url, init } = lastCall(fetchMock)
    expect(url).toBe('/api/users/3/identities')
    expect(init.method).toBe('PUT')
    expect(header(init, 'x-csrf-token')).toBe(CSRF_TOKEN)
    expect(header(init, 'Content-Type')).toBe('application/json')
    expect(JSON.parse(init.body as string)).toEqual({
      slug: 'jake',
      github_usernames: ['jakebarnby'],
      sms_number: '',
    })
    expect(result).toEqual(jake)
  })

  test('resetUserIdentities deletes with the CSRF header and no body', async () => {
    const fetchMock = mockFetch({ ...jake, origin: 'config' })
    const result = await resetUserIdentities(3)
    const { url, init } = lastCall(fetchMock)
    expect(url).toBe('/api/users/3/identities')
    expect(init.method).toBe('DELETE')
    expect(header(init, 'x-csrf-token')).toBe(CSRF_TOKEN)
    expect(init.body).toBeUndefined()
    expect(result.origin).toBe('config')
  })

  test('a 400 rejects with ApiError carrying the status, message and violations', async () => {
    const violations: IdentityViolation[] = [
      { field: 'discord_id', message: 'Discord ID must be a 17-20 digit Discord user ID' },
    ]
    mockFetch({ error: 'Invalid identities', violations }, 400)
    const error = await captureError(updateUserIdentities(3, { discord_id: '12' }))
    expect(error.status).toBe(400)
    expect(error.message).toBe('Invalid identities')
    expect(error.violations).toEqual(violations)
  })

  test('a 409 without violations rejects with an empty violation list', async () => {
    mockFetch({ error: 'GitHub username already belongs to another person' }, 409)
    const error = await captureError(updateUserIdentities(3, { github_usernames: ['taken'] }))
    expect(error.status).toBe(409)
    expect(error.message).toBe('GitHub username already belongs to another person')
    expect(error.violations).toEqual([])
  })

  test('an error without a JSON body falls back to a status message', async () => {
    globalThis.fetch = mock(() =>
      Promise.resolve({
        ok: false,
        status: 502,
        json: () => Promise.reject(new SyntaxError('Unexpected end of JSON input')),
      } as Response),
    ) as unknown as typeof fetch
    const error = await captureError(resetUserIdentities(3))
    expect(error.status).toBe(502)
    expect(error.message).toBe('Failed to delete /api/users/3/identities: 502')
    expect(error.violations).toEqual([])
  })

  test('a 401 triggers the unauthorized callback', async () => {
    const unauthorized = mock(() => {})
    setOnUnauthorized(unauthorized)
    mockFetch({ error: 'Unauthorized' }, 401)
    await expect(updateUserIdentities(3, { slug: 'jake' })).rejects.toThrow('Unauthorized')
    expect(unauthorized).toHaveBeenCalledTimes(1)
  })

  test('createUser without a password posts identities and returns the invite', async () => {
    const created = {
      id: 7,
      email: 'amy@example.com',
      name: 'Amy',
      role: 'viewer',
      avatar_url: null,
      status: 'invited',
      invite_expires_at: invite.expires_at,
      created_at: '',
      updated_at: '',
      invite,
    }
    const fetchMock = mockFetch(created, 201)
    const result = await createUser({
      email: 'amy@example.com',
      name: 'Amy',
      role: 'viewer',
      identities: { github_usernames: ['amy'] },
    })
    const { url, init } = lastCall(fetchMock)
    expect(url).toBe('/api/users')
    expect(init.method).toBe('POST')
    expect(header(init, 'x-csrf-token')).toBe(CSRF_TOKEN)
    const body = JSON.parse(init.body as string)
    expect(body).toEqual({
      email: 'amy@example.com',
      name: 'Amy',
      role: 'viewer',
      identities: { github_usernames: ['amy'] },
    })
    expect('password' in body).toBe(false)
    expect(result.status).toBe('invited')
    expect(result.invite).toEqual(invite)
  })

  test('createUser rejects with ApiError on a validation failure', async () => {
    mockFetch({ error: 'Password must be 8-72 bytes' }, 400)
    const error = await captureError(createUser({ email: 'a@example.com', name: 'A', role: 'viewer', password: 'short' }))
    expect(error.status).toBe(400)
    expect(error.message).toBe('Password must be 8-72 bytes')
  })

  test('updateUser puts the sparse update with the CSRF header', async () => {
    const fetchMock = mockFetch({ id: 7, email: 'amy@example.com', name: 'Amy', role: 'admin' })
    await updateUser(7, { role: 'admin' })
    const { url, init } = lastCall(fetchMock)
    expect(url).toBe('/api/users/7')
    expect(init.method).toBe('PUT')
    expect(header(init, 'x-csrf-token')).toBe(CSRF_TOKEN)
    expect(JSON.parse(init.body as string)).toEqual({ role: 'admin' })
  })

  test('updateUser surfaces the server error message', async () => {
    mockFetch({ error: 'A user with this email already exists' }, 409)
    const error = await captureError(updateUser(7, { email: 'taken@example.com' }))
    expect(error.status).toBe(409)
    expect(error.message).toBe('A user with this email already exists')
  })

  test('deleteUser deletes with the CSRF header and surfaces the server error message', async () => {
    const fetchMock = mockFetch({ error: 'Admin access required' }, 403)
    const error = await captureError(deleteUser(7))
    const { url, init } = lastCall(fetchMock)
    expect(url).toBe('/api/users/7')
    expect(init.method).toBe('DELETE')
    expect(header(init, 'x-csrf-token')).toBe(CSRF_TOKEN)
    expect(error.status).toBe(403)
    expect(error.message).toBe('Admin access required')
  })

  test('issueInvite posts to the user invite route', async () => {
    const fetchMock = mockFetch(invite)
    const result = await issueInvite(7)
    const { url, init } = lastCall(fetchMock)
    expect(url).toBe('/api/users/7/invite')
    expect(init.method).toBe('POST')
    expect(header(init, 'x-csrf-token')).toBe(CSRF_TOKEN)
    expect(init.body).toBeUndefined()
    expect(result).toEqual(invite)
  })

  test('issueInvite surfaces a 409 for an active user', async () => {
    mockFetch({ error: 'User is already active' }, 409)
    const error = await captureError(issueInvite(1))
    expect(error.status).toBe(409)
    expect(error.message).toBe('User is already active')
  })

  test('revokeInvite deletes the user invite', async () => {
    const fetchMock = mockFetch({ message: 'Invite revoked' })
    await expect(revokeInvite(7)).resolves.toBeUndefined()
    const { url, init } = lastCall(fetchMock)
    expect(url).toBe('/api/users/7/invite')
    expect(init.method).toBe('DELETE')
    expect(header(init, 'x-csrf-token')).toBe(CSRF_TOKEN)
  })

  test('inspectInvite posts the token', async () => {
    const details = { name: 'Amy', email: 'amy@example.com', expires_at: invite.expires_at }
    const fetchMock = mockFetch(details)
    const result = await inspectInvite('cldi_abc')
    const { url, init } = lastCall(fetchMock)
    expect(url).toBe('/api/auth/invite/inspect')
    expect(init.method).toBe('POST')
    expect(header(init, 'x-csrf-token')).toBe(CSRF_TOKEN)
    expect(JSON.parse(init.body as string)).toEqual({ token: 'cldi_abc' })
    expect(result).toEqual(details)
  })

  test('inspectInvite surfaces the generic 404', async () => {
    mockFetch({ error: 'This invite link is invalid or has expired' }, 404)
    const error = await captureError(inspectInvite('cldi_gone'))
    expect(error.status).toBe(404)
    expect(error.message).toBe('This invite link is invalid or has expired')
  })

  test('acceptInvite posts the token and password', async () => {
    const fetchMock = mockFetch({ message: 'Invite accepted' })
    await expect(acceptInvite('cldi_abc', 'correct horse')).resolves.toBeUndefined()
    const { url, init } = lastCall(fetchMock)
    expect(url).toBe('/api/auth/invite/accept')
    expect(init.method).toBe('POST')
    expect(header(init, 'x-csrf-token')).toBe(CSRF_TOKEN)
    expect(JSON.parse(init.body as string)).toEqual({ token: 'cldi_abc', password: 'correct horse' })
  })

  test('inviteAbsoluteUrl prefers the server invite_url', () => {
    const url = 'https://claudear.example.com/invite#cldi_abc'
    expect(inviteAbsoluteUrl({ ...invite, invite_url: url })).toBe(url)
  })

  test('inviteAbsoluteUrl falls back to the current origin plus invite_link', () => {
    expect(inviteAbsoluteUrl(invite)).toBe(`${window.location.origin}/invite#cldi_abc`)
  })

  test('SENSITIVE_IDENTITY_FIELDS mirrors the server sensitive set', () => {
    expect([...SENSITIVE_IDENTITY_FIELDS].sort()).toEqual(
      ['email', 'push_user_key', 'sms_number', 'telegram_chat_id', 'whatsapp_number'],
    )
  })
})
