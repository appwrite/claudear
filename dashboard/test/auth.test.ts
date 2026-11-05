import { describe, expect, test, mock, afterEach } from 'bun:test'
import { login, logout, getMe, fetchUsers, createUser, deleteUser } from '../src/lib/api'

function mockFetch(data: unknown, status = 200) {
  globalThis.fetch = mock(() =>
    Promise.resolve({
      ok: status >= 200 && status < 300,
      status,
      json: () => Promise.resolve(data),
    })
  ) as unknown as typeof fetch
}

describe('auth api', () => {
  const originalFetch = globalThis.fetch

  afterEach(() => {
    globalThis.fetch = originalFetch
  })

  test('login sends credentials and returns user', async () => {
    const mockResponse = { user: { id: 1, email: 'admin@test.com', name: 'Admin', role: 'admin' } }
    mockFetch(mockResponse)

    const result = await login('admin@test.com', 'password')
    expect(result.user.email).toBe('admin@test.com')
    expect(fetch).toHaveBeenCalledTimes(1)
  })

  test('login throws on 401', async () => {
    mockFetch(null, 401)
    await expect(login('bad@test.com', 'wrong')).rejects.toThrow()
  })

  test('logout calls post', async () => {
    mockFetch(null, 200)
    await logout()
    expect(fetch).toHaveBeenCalledTimes(1)
  })

  test('getMe returns current user', async () => {
    mockFetch({ id: 1, email: 'admin@test.com', name: 'Admin', role: 'admin' })
    const user = await getMe()
    expect(user.email).toBe('admin@test.com')
  })

  test('getMe throws on 401', async () => {
    mockFetch(null, 401)
    await expect(getMe()).rejects.toThrow()
  })

  test('fetchUsers returns user list', async () => {
    mockFetch([
      { id: 1, email: 'a@test.com', name: 'A', role: 'admin', created_at: '', updated_at: '' },
    ])
    const users = await fetchUsers()
    expect(users).toHaveLength(1)
  })

  test('createUser sends user data', async () => {
    mockFetch(
      { id: 2, email: 'new@test.com', name: 'New', role: 'viewer', created_at: '', updated_at: '' },
      201
    )
    const user = await createUser({ email: 'new@test.com', password: 'pass', name: 'New', role: 'viewer' })
    expect(user.email).toBe('new@test.com')
  })

  test('deleteUser sends delete request', async () => {
    mockFetch(null, 204)
    await deleteUser(1)
    expect(fetch).toHaveBeenCalledTimes(1)
  })
})
