import { useId, useState } from 'react'
import useSWR from 'swr'
import {
  ADMIN_ROLE,
  ApiError,
  createUser,
  createUserToken,
  deleteUser,
  fetchIdentities,
  fetchUsers,
  fetchUserTokens,
  inviteAbsoluteUrl,
  issueInvite,
  revokeInvite,
  revokeToken,
  updateUser,
  type ApiToken,
  type CreatedToken,
  type CreatedUser,
  type Role,
  type UserIdentities,
  type UserInvite,
  type UserRecord,
} from '../lib/api'
import { formatDate, parseUTCDate } from '../lib/formatters'
import { useAuth } from '../lib/auth'
import { passwordLengthProblem } from '../lib/password'
import { UsersTableSkeleton } from '../components/shared/page-skeletons'
import { SecretReveal } from '../components/shared/secret-reveal'
import { TimeAgo } from '../components/shared/time-ago'
import { Badge } from '../components/ui/badge'
import { IdentityForm, IDENTITIES_KEY } from '../components/users/identity-form'
import { IdentitySummary } from '../components/users/identity-summary'
import { Fingerprint, KeyRound, Link2, Pencil, Plus, Trash2, Unlink, X } from 'lucide-react'

const MINUTE_MS = 60_000
const HOUR_MS = 60 * MINUTE_MS
const DAY_MS = 24 * HOUR_MS

interface IssuedInvite {
  userId: number
  name: string
  url: string
  expiresAt: string
  copied: boolean | null
  replacesPrevious: boolean
}

function issuedInvite(user: UserRecord, invite: UserInvite, copied: boolean | null, replacesPrevious: boolean): IssuedInvite {
  return { userId: user.id, name: user.name, url: inviteAbsoluteUrl(invite), expiresAt: invite.expires_at, copied, replacesPrevious }
}

export function describeInviteExpiry(expiresAt: string | null, now: number = Date.now()): string {
  if (expiresAt === null) return 'no open invite'
  const remaining = parseUTCDate(expiresAt).getTime() - now
  if (remaining <= 0) return 'expired'
  const relative = new Intl.RelativeTimeFormat(undefined, { numeric: 'always' })
  if (remaining >= DAY_MS) return `expires ${relative.format(Math.round(remaining / DAY_MS), 'day')}`
  if (remaining >= HOUR_MS) return `expires ${relative.format(Math.round(remaining / HOUR_MS), 'hour')}`
  return `expires ${relative.format(Math.max(1, Math.round(remaining / MINUTE_MS)), 'minute')}`
}

export function hasOpenInvite(user: UserRecord, now: number = Date.now()): boolean {
  return user.invite_expires_at !== null && parseUTCDate(user.invite_expires_at).getTime() > now
}

const VIEWER_ROLE: Role = 'viewer'

const ROLE_LABELS: Record<Role, string> = {
  admin: 'Admin',
  viewer: 'Viewer',
}

function isRole(value: string): value is Role {
  return Object.prototype.hasOwnProperty.call(ROLE_LABELS, value)
}

function failureMessage(error: unknown, fallback: string): string {
  return error instanceof ApiError && error.status < 500 ? error.message : fallback
}

async function copyToClipboard(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    return false
  }
}

export default function UsersPage() {
  const { user: currentUser } = useAuth()
  if (!currentUser) return null
  if (currentUser.role !== ADMIN_ROLE) return <MyIdentities userId={currentUser.id} />
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
  const [invite, setInvite] = useState<IssuedInvite | null>(null)

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
    } catch (error: unknown) {
      setActionError(failureMessage(error, 'Failed to delete user'))
    }
  }

  const handleCopyInvite = async (user: UserRecord) => {
    const replacesPrevious = hasOpenInvite(user)
    if (
      replacesPrevious
      && !confirm(`Create a new invite link for ${user.name}? The link sent earlier stops working.`)
    ) return
    setActionError('')
    let issued: UserInvite
    try {
      issued = await issueInvite(user.id)
    } catch (error: unknown) {
      setActionError(failureMessage(error, 'Failed to create an invite link'))
      return
    }
    const copied = await copyToClipboard(inviteAbsoluteUrl(issued))
    setInvite(issuedInvite(user, issued, copied, replacesPrevious))
    void mutate().catch(() => setActionError('Failed to refresh users'))
  }

  const handleRevokeInvite = async (user: UserRecord) => {
    if (!confirm(`Revoke the invite for ${user.name}? Their link will stop working.`)) return
    setActionError('')
    try {
      await revokeInvite(user.id)
      setInvite((current) => (current?.userId === user.id ? null : current))
      await mutate()
    } catch (error: unknown) {
      setActionError(failureMessage(error, 'Failed to revoke invite'))
    }
  }

  const handleSaved = (created: CreatedUser | null) => {
    setActionError('')
    if (created?.invite) {
      setInvite(issuedInvite(created, created.invite, null, false))
      setEditingUser(created)
    } else {
      setShowForm(false)
    }
    void mutate().catch(() => setActionError('Failed to refresh users'))
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

      {invite && <InviteNotice invite={invite} onDismiss={() => setInvite(null)} />}

      {showForm && (
        <UserForm
          key={editingUser?.id ?? 'new'}
          user={editingUser}
          onSave={handleSaved}
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
                  <td className="p-3">
                    <div className="flex flex-wrap items-center gap-2">
                      <span>{u.name}</span>
                      {u.status === 'invited' && <InviteStatus expiresAt={u.invite_expires_at} />}
                    </div>
                  </td>
                  <td className="p-3 text-muted-foreground">{u.email}</td>
                  <td className="p-3">
                    <span className={`px-2 py-0.5 rounded-full text-xs font-medium ${
                      u.role === ADMIN_ROLE ? 'bg-primary/10 text-primary' : 'bg-muted text-muted-foreground'
                    }`}>{u.role}</span>
                  </td>
                  <td className="p-3"><IdentitySummary identities={identitiesByUser.get(u.id)} /></td>
                  <td className="p-3 text-muted-foreground">{parseUTCDate(u.created_at).toLocaleDateString()}</td>
                  <td className="p-3 text-right space-x-1">
                    {u.status === 'invited' && (
                      <>
                        <button
                          onClick={() => handleCopyInvite(u)}
                          className="p-1.5 rounded hover:bg-muted"
                          title="Copy invite link"
                          aria-label={`Copy a new invite link for ${u.name}`}
                        >
                          <Link2 className="h-4 w-4" />
                        </button>
                        {u.invite_expires_at !== null && (
                          <button
                            onClick={() => handleRevokeInvite(u)}
                            className="p-1.5 rounded hover:bg-destructive/10 text-destructive"
                            title="Revoke invite"
                            aria-label={`Revoke the invite for ${u.name}`}
                          >
                            <Unlink className="h-4 w-4" />
                          </button>
                        )}
                      </>
                    )}
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

function InviteStatus({ expiresAt }: { expiresAt: string | null }) {
  const expiry = describeInviteExpiry(expiresAt)
  const lapsed = expiry === 'expired' || expiresAt === null
  return (
    <>
      <Badge variant="outline" className="border-amber-500/30 bg-amber-500/10 text-amber-600 dark:text-amber-400">
        Invited
      </Badge>
      <span className={`text-xs ${lapsed ? 'text-destructive' : 'text-muted-foreground'}`}>{expiry}</span>
    </>
  )
}

function InviteNotice({ invite, onDismiss }: { invite: IssuedInvite; onDismiss: () => void }) {
  return (
    <div role="status" className="bg-primary/5 border border-primary/20 rounded-md p-3 space-y-2">
      <div className="flex items-start justify-between gap-2">
        <div className="space-y-1">
          <div className="text-sm font-medium text-primary">Invite link for {invite.name}</div>
          <p className="text-xs text-muted-foreground">
            {invite.copied === true && 'Copied to clipboard. '}
            {invite.copied === false && "Couldn't access the clipboard, so copy the link below manually. "}
            {invite.copied === null && `Send this link to ${invite.name} so they can set a password. `}
            {invite.replacesPrevious && 'Any previous invite link for this user no longer works. '}
            It works once and expires {formatDate(invite.expiresAt)}. It won't be shown again.
          </p>
        </div>
        <button onClick={onDismiss} className="p-1 rounded hover:bg-muted" title="Dismiss">
          <X className="h-4 w-4" />
        </button>
      </div>
      <SecretReveal secret={invite.url} noun="link" />
    </div>
  )
}

function UserForm({
  user,
  onSave,
  onCancel,
}: {
  user: UserRecord | null
  onSave: (created: CreatedUser | null) => void
  onCancel: () => void
}) {
  const fieldId = useId()
  const [email, setEmail] = useState(user?.email ?? '')
  const [name, setName] = useState(user?.name ?? '')
  const [password, setPassword] = useState('')
  const [role, setRole] = useState<Role>(user?.role ?? VIEWER_ROLE)
  const [error, setError] = useState('')
  const [saving, setSaving] = useState(false)

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    const lengthProblem = password ? passwordLengthProblem(password) : null
    if (lengthProblem) {
      setError(lengthProblem)
      return
    }
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
        onSave(null)
      } else {
        onSave(await createUser({ email, name, role, password: password || undefined }))
      }
    } catch (failure: unknown) {
      setError(failureMessage(failure, user ? 'Failed to update user' : 'Failed to create user'))
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
          <label htmlFor={`${fieldId}-name`} className="text-sm font-medium">Name</label>
          <input
            id={`${fieldId}-name`}
            value={name} onChange={(e) => setName(e.target.value)} required
            className="w-full px-3 py-2 border rounded-md bg-background text-sm focus:outline-none focus:ring-2 focus:ring-primary"
          />
        </div>
        <div className="space-y-1">
          <label htmlFor={`${fieldId}-email`} className="text-sm font-medium">Email</label>
          <input
            id={`${fieldId}-email`}
            type="email" value={email} onChange={(e) => setEmail(e.target.value)} required
            className="w-full px-3 py-2 border rounded-md bg-background text-sm focus:outline-none focus:ring-2 focus:ring-primary"
          />
        </div>
        <div className="space-y-1">
          <label htmlFor={`${fieldId}-password`} className="text-sm font-medium">Password</label>
          <input
            id={`${fieldId}-password`}
            type="password" value={password} onChange={(e) => setPassword(e.target.value)}
            autoComplete="new-password"
            placeholder={passwordPlaceholder(user)}
            className="w-full px-3 py-2 border rounded-md bg-background text-sm focus:outline-none focus:ring-2 focus:ring-primary"
          />
        </div>
        <div className="space-y-1">
          <label htmlFor={`${fieldId}-role`} className="text-sm font-medium">Role</label>
          <select
            id={`${fieldId}-role`}
            value={role} onChange={(e) => { if (isRole(e.target.value)) setRole(e.target.value) }}
            className="w-full px-3 py-2 border rounded-md bg-background text-sm focus:outline-none focus:ring-2 focus:ring-primary"
          >
            {Object.entries(ROLE_LABELS).map(([value, label]) => (
              <option key={value} value={value}>{label}</option>
            ))}
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

function passwordPlaceholder(user: UserRecord | null): string {
  if (!user) return 'Leave blank to invite'
  if (user.status === 'invited') return 'Leave blank to keep the invite'
  return 'Leave blank to keep'
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
