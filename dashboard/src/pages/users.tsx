import { useState } from 'react'
import useSWR from 'swr'
import {
  fetchUsers, createUser, updateUser, deleteUser,
  fetchUserTokens, createUserToken, revokeToken, fetchIdentities,
  type UserRecord, type ApiToken, type CreatedToken, type UserIdentities,
} from '../lib/api'
import { parseUTCDate } from '../lib/formatters'
import { useAuth } from '../lib/auth'
import { UsersTableSkeleton } from '../components/shared/page-skeletons'
import { TimeAgo } from '../components/shared/time-ago'
import { IdentityForm, IDENTITIES_KEY } from '../components/users/identity-form'
import { IdentitySummary } from '../components/users/identity-summary'
import { Plus, Pencil, Trash2, X, Copy, Check, KeyRound, Fingerprint } from 'lucide-react'

export default function UsersPage() {
  const { user: currentUser } = useAuth()
  if (!currentUser) return null
  if (currentUser.role !== 'admin') return <MyIdentities userId={currentUser.id} />
  return <UserDirectory currentUserId={currentUser.id} />
}

function MyIdentities({ userId }: { userId: number }) {
  return (
    <div className="space-y-6">
      <title>My identities — Claudear</title>
      <div>
        <h2 className="text-2xl font-bold">My identities</h2>
        <p className="text-sm text-muted-foreground">
          The handles Claudear uses to recognise you across trackers and to reach you in chat.
        </p>
      </div>
      <div className="border rounded-lg p-6 bg-card">
        <IdentityForm userId={userId} canEditSlug={false} />
      </div>
    </div>
  )
}

function UserDirectory({ currentUserId }: { currentUserId: number }) {
  const [showForm, setShowForm] = useState(false)
  const [editingUser, setEditingUser] = useState<UserRecord | null>(null)
  const [actionError, setActionError] = useState('')

  const {
    data: users = [],
    error: loadError,
    isLoading,
    mutate,
  } = useSWR<UserRecord[]>('users', fetchUsers)
  const { data: identities = [] } = useSWR<UserIdentities[]>(IDENTITIES_KEY, fetchIdentities)
  const identitiesByUser = new Map(identities.map((entry) => [entry.user_id, entry]))

  const handleDelete = async (id: number) => {
    if (!confirm('Delete this user?')) return
    setActionError('')
    try {
      await deleteUser(id)
      await mutate()
    } catch {
      setActionError('Failed to delete user')
    }
  }

  return (
    <div className="space-y-6">
      <title>Users — Claudear</title>
      <div className="flex items-center justify-between">
        <h2 className="text-2xl font-bold">Users</h2>
        <button
          onClick={() => { setEditingUser(null); setShowForm(true) }}
          className="flex items-center gap-2 px-3 py-2 bg-primary text-primary-foreground rounded-md text-sm font-medium hover:bg-primary/90"
        >
          <Plus className="h-4 w-4" /> Add User
        </button>
      </div>

      {(actionError || loadError) && (
        <div className="bg-destructive/10 text-destructive text-sm p-3 rounded-md">
          {actionError || 'Failed to load users'}
        </div>
      )}

      {showForm && (
        <UserForm
          key={editingUser?.id ?? 'new'}
          user={editingUser}
          onSave={() => {
            setShowForm(false)
            setActionError('')
            void mutate().catch(() => setActionError('Failed to refresh users'))
          }}
          onCancel={() => setShowForm(false)}
        />
      )}

      {isLoading ? (
        <UsersTableSkeleton rows={5} />
      ) : (
        <div className="border rounded-lg overflow-hidden">
          <table className="w-full text-sm">
            <thead className="bg-muted/50">
              <tr>
                <th className="text-left p-3 font-medium">Name</th>
                <th className="text-left p-3 font-medium">Email</th>
                <th className="text-left p-3 font-medium">Role</th>
                <th className="text-left p-3 font-medium">Identities</th>
                <th className="text-left p-3 font-medium">Created</th>
                <th className="text-right p-3 font-medium">Actions</th>
              </tr>
            </thead>
            <tbody>
              {users.map((u) => (
                <tr key={u.id} className="border-t">
                  <td className="p-3">{u.name}</td>
                  <td className="p-3 text-muted-foreground">{u.email}</td>
                  <td className="p-3">
                    <span className={`px-2 py-0.5 rounded-full text-xs font-medium ${
                      u.role === 'admin' ? 'bg-primary/10 text-primary' : 'bg-muted text-muted-foreground'
                    }`}>{u.role}</span>
                  </td>
                  <td className="p-3"><IdentitySummary identities={identitiesByUser.get(u.id)} /></td>
                  <td className="p-3 text-muted-foreground">{parseUTCDate(u.created_at).toLocaleDateString()}</td>
                  <td className="p-3 text-right space-x-1">
                    <button
                      onClick={() => { setEditingUser(u); setShowForm(true) }}
                      className="p-1.5 rounded hover:bg-muted"
                      title="Edit"
                    >
                      <Pencil className="h-4 w-4" />
                    </button>
                    {u.id !== currentUserId && (
                      <button
                        onClick={() => handleDelete(u.id)}
                        className="p-1.5 rounded hover:bg-destructive/10 text-destructive"
                        title="Delete"
                      >
                        <Trash2 className="h-4 w-4" />
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  )
}

function UserForm({
  user,
  onSave,
  onCancel,
}: {
  user: UserRecord | null
  onSave: () => void
  onCancel: () => void
}) {
  const [email, setEmail] = useState(user?.email ?? '')
  const [name, setName] = useState(user?.name ?? '')
  const [password, setPassword] = useState('')
  const [role, setRole] = useState(user?.role ?? 'viewer')
  const [error, setError] = useState('')
  const [saving, setSaving] = useState(false)

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    setError('')
    setSaving(true)
    try {
      if (user) {
        await updateUser(user.id, {
          email: email !== user.email ? email : undefined,
          name: name !== user.name ? name : undefined,
          role: role !== user.role ? role : undefined,
          password: password || undefined,
        })
      } else {
        if (!password) { setError('Password is required'); setSaving(false); return }
        await createUser({ email, password, name, role })
      }
      onSave()
    } catch {
      setError(user ? 'Failed to update user' : 'Failed to create user')
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className="border rounded-lg p-4 bg-card">
      <div className="flex items-center justify-between mb-4">
        <h3 className="font-medium">{user ? 'Edit User' : 'New User'}</h3>
        <button onClick={onCancel} className="p-1 rounded hover:bg-muted">
          <X className="h-4 w-4" />
        </button>
      </div>
      {error && (
        <div className="bg-destructive/10 text-destructive text-sm p-3 rounded-md mb-4">{error}</div>
      )}
      <form onSubmit={handleSubmit} className="grid grid-cols-2 gap-4">
        <div className="space-y-1">
          <label className="text-sm font-medium">Name</label>
          <input
            value={name} onChange={(e) => setName(e.target.value)} required
            className="w-full px-3 py-2 border rounded-md bg-background text-sm focus:outline-none focus:ring-2 focus:ring-primary"
          />
        </div>
        <div className="space-y-1">
          <label className="text-sm font-medium">Email</label>
          <input
            type="email" value={email} onChange={(e) => setEmail(e.target.value)} required
            className="w-full px-3 py-2 border rounded-md bg-background text-sm focus:outline-none focus:ring-2 focus:ring-primary"
          />
        </div>
        <div className="space-y-1">
          <label className="text-sm font-medium">Password{user ? ' (leave blank to keep)' : ''}</label>
          <input
            type="password" value={password} onChange={(e) => setPassword(e.target.value)}
            required={!user}
            className="w-full px-3 py-2 border rounded-md bg-background text-sm focus:outline-none focus:ring-2 focus:ring-primary"
          />
        </div>
        <div className="space-y-1">
          <label className="text-sm font-medium">Role</label>
          <select
            value={role} onChange={(e) => setRole(e.target.value)}
            className="w-full px-3 py-2 border rounded-md bg-background text-sm focus:outline-none focus:ring-2 focus:ring-primary"
          >
            <option value="admin">Admin</option>
            <option value="viewer">Viewer</option>
          </select>
        </div>
        <div className="col-span-2 flex justify-end gap-2">
          <button type="button" onClick={onCancel} className="px-3 py-2 border rounded-md text-sm hover:bg-muted">
            Cancel
          </button>
          <button type="submit" disabled={saving} className="px-3 py-2 bg-primary text-primary-foreground rounded-md text-sm font-medium hover:bg-primary/90 disabled:opacity-50">
            {saving ? 'Saving...' : user ? 'Update' : 'Create'}
          </button>
        </div>
      </form>

      {user ? (
        <>
          <div className="mt-6 pt-4 border-t space-y-3">
            <div className="flex items-center gap-2">
              <Fingerprint className="h-4 w-4 text-muted-foreground" />
              <h4 className="font-medium text-sm">Identities</h4>
            </div>
            <IdentityForm key={user.id} userId={user.id} canEditSlug />
          </div>
          <UserTokens key={user.id} userId={user.id} />
        </>
      ) : (
        <p className="mt-6 pt-4 border-t text-xs text-muted-foreground">
          Save the user first, then re-open to create API tokens for them.
        </p>
      )}
    </div>
  )
}

function UserTokens({ userId }: { userId: number }) {
  const { data: tokens = [], isLoading, mutate } = useSWR<ApiToken[]>(
    ['user-tokens', userId],
    () => fetchUserTokens(userId),
  )
  const [name, setName] = useState('')
  const [creating, setCreating] = useState(false)
  const [error, setError] = useState('')
  const [created, setCreated] = useState<CreatedToken | null>(null)

  const handleCreate = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!name.trim()) return
    setError('')
    setCreating(true)
    try {
      const token = await createUserToken(userId, { name: name.trim() })
      setCreated(token)
      setName('')
      await mutate()
    } catch {
      setError('Failed to create token')
    } finally {
      setCreating(false)
    }
  }

  const handleRevoke = async (id: string) => {
    if (!confirm('Revoke this token? Clients using it will stop working.')) return
    setError('')
    try {
      await revokeToken(id)
      await mutate()
    } catch {
      setError('Failed to revoke token')
    }
  }

  return (
    <div className="mt-6 pt-4 border-t space-y-3">
      <div className="flex items-center gap-2">
        <KeyRound className="h-4 w-4 text-muted-foreground" />
        <h4 className="font-medium text-sm">API Tokens</h4>
      </div>
      <p className="text-xs text-muted-foreground">
        Tokens let this user's tools (e.g. the MCP search server) authenticate as them.
        The secret is shown once, at creation.
      </p>

      {error && (
        <div className="bg-destructive/10 text-destructive text-sm p-2 rounded-md">{error}</div>
      )}

      {created && (
        <div className="bg-primary/5 border border-primary/20 rounded-md p-3 space-y-2">
          <div className="text-xs font-medium text-primary">
            Copy this token now — it won't be shown again.
          </div>
          <SecretReveal secret={created.secret} />
        </div>
      )}

      <form onSubmit={handleCreate} className="flex items-center gap-2">
        <input
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="Token name (e.g. laptop, ci)"
          className="flex-1 px-3 py-2 border rounded-md bg-background text-sm focus:outline-none focus:ring-2 focus:ring-primary"
        />
        <button
          type="submit"
          disabled={creating || !name.trim()}
          className="flex items-center gap-1.5 px-3 py-2 bg-primary text-primary-foreground rounded-md text-sm font-medium hover:bg-primary/90 disabled:opacity-50"
        >
          <Plus className="h-4 w-4" /> Create
        </button>
      </form>

      {isLoading ? (
        <div className="text-xs text-muted-foreground">Loading tokens…</div>
      ) : tokens.length === 0 ? (
        <div className="text-xs text-muted-foreground">No tokens yet.</div>
      ) : (
        <div className="border rounded-md overflow-hidden">
          <table className="w-full text-sm">
            <thead className="bg-muted/50">
              <tr>
                <th className="text-left p-2 font-medium">Name</th>
                <th className="text-left p-2 font-medium">Prefix</th>
                <th className="text-left p-2 font-medium">Created</th>
                <th className="text-left p-2 font-medium">Last used</th>
                <th className="text-right p-2 font-medium">Actions</th>
              </tr>
            </thead>
            <tbody>
              {tokens.map((t) => (
                <tr key={t.id} className="border-t">
                  <td className="p-2">{t.name}</td>
                  <td className="p-2 font-mono text-xs text-muted-foreground">{t.token_prefix}…</td>
                  <td className="p-2 text-muted-foreground">{parseUTCDate(t.created_at).toLocaleDateString()}</td>
                  <td className="p-2 text-muted-foreground">
                    {t.last_used_at ? <TimeAgo date={t.last_used_at} /> : 'never'}
                  </td>
                  <td className="p-2 text-right">
                    <button
                      onClick={() => handleRevoke(t.id)}
                      className="p-1.5 rounded hover:bg-destructive/10 text-destructive"
                      title="Revoke"
                    >
                      <Trash2 className="h-4 w-4" />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  )
}

function SecretReveal({ secret }: { secret: string }) {
  const [state, setState] = useState<'idle' | 'copied' | 'failed'>('idle')
  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(secret)
      setState('copied')
      setTimeout(() => setState('idle'), 2000)
    } catch {
      // Clipboard access can be denied; tell the user to copy manually rather
      // than falsely reporting success (the secret is shown only once).
      setState('failed')
    }
  }
  return (
    <div className="space-y-1">
      <div className="flex items-center gap-2">
        <code className="flex-1 px-2 py-1.5 bg-muted rounded font-mono text-xs break-all select-all">{secret}</code>
        <button
          onClick={handleCopy}
          className="flex items-center gap-1 px-2 py-1.5 border rounded-md text-xs hover:bg-muted shrink-0"
          title="Copy"
        >
          {state === 'copied' ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
          {state === 'copied' ? 'Copied' : 'Copy'}
        </button>
      </div>
      {state === 'failed' && (
        <div className="text-xs text-destructive">
          Couldn't access the clipboard — select the token above and copy it manually.
        </div>
      )}
    </div>
  )
}
