import { useState } from 'react'
import useSWR from 'swr'
import { KeyRound, Plus, Trash2 } from 'lucide-react'
import {
  ApiError,
  createMyToken,
  fetchMyTokens,
  revokeToken,
  type ApiToken,
  type CreatedToken,
} from '../../lib/api'
import { parseUTCDate } from '../../lib/formatters'
import { Select } from '../ui/select'
import { SecretReveal } from '../shared/secret-reveal'
import { TimeAgo } from '../shared/time-ago'

export const MY_TOKENS_KEY = 'my-tokens'

type TokenLifetime = 'never' | '30' | '90' | '365'

const lifetimeOptions: { value: TokenLifetime; label: string }[] = [
  { value: 'never', label: 'No expiry' },
  { value: '30', label: '30 days' },
  { value: '90', label: '90 days' },
  { value: '365', label: '1 year' },
]

const millisecondsPerDay = 24 * 60 * 60 * 1000

function isTokenLifetime(value: string): value is TokenLifetime {
  return lifetimeOptions.some(option => option.value === value)
}

function expiryFor(lifetime: TokenLifetime): string | undefined {
  if (lifetime === 'never') return undefined
  return new Date(Date.now() + Number(lifetime) * millisecondsPerDay).toISOString()
}

function errorMessage(error: unknown, fallback: string): string {
  return error instanceof ApiError ? error.message : fallback
}

export function PersonalTokensCard() {
  const { data: tokens = [], error: loadError, isLoading, mutate } = useSWR<ApiToken[]>(
    MY_TOKENS_KEY,
    fetchMyTokens,
  )
  const [name, setName] = useState('')
  const [lifetime, setLifetime] = useState<TokenLifetime>('never')
  const [creating, setCreating] = useState(false)
  const [error, setError] = useState('')
  const [created, setCreated] = useState<CreatedToken | null>(null)

  const handleCreate = async (e: React.FormEvent) => {
    e.preventDefault()
    const trimmed = name.trim()
    if (!trimmed) return
    setError('')
    setCreating(true)
    try {
      const token = await createMyToken({ name: trimmed, expires_at: expiryFor(lifetime) })
      setCreated(token)
      setName('')
      await mutate()
    } catch (createError) {
      setError(errorMessage(createError, 'Failed to create token'))
    } finally {
      setCreating(false)
    }
  }

  const handleRevoke = async (token: ApiToken) => {
    if (!confirm(`Revoke "${token.name}"? Clients using it will stop working.`)) return
    setError('')
    try {
      await revokeToken(token.id)
      if (created?.id === token.id) setCreated(null)
      await mutate()
    } catch (revokeError) {
      setError(errorMessage(revokeError, 'Failed to revoke token'))
    }
  }

  return (
    <div className="border rounded-lg p-6 bg-card space-y-4">
      <div className="flex items-center gap-2">
        <KeyRound className="h-4 w-4 text-muted-foreground" />
        <h3 className="font-medium">Personal access tokens</h3>
      </div>
      <p className="text-sm text-muted-foreground">
        Tokens authenticate your tools against the MCP server endpoint (<code className="font-mono">/mcp</code>).
        Send one as <code className="font-mono">Authorization: Bearer &lt;token&gt;</code>. The secret is shown
        once, when the token is created.
      </p>

      {error && (
        <div role="alert" className="bg-destructive/10 text-destructive text-sm p-2 rounded-md">{error}</div>
      )}

      {created && (
        <div className="bg-primary/5 border border-primary/20 rounded-md p-3 space-y-2">
          <div className="text-xs font-medium text-primary">
            Copy "{created.name}" now. It won't be shown again.
          </div>
          <SecretReveal secret={created.secret} />
          <button
            type="button"
            onClick={() => setCreated(null)}
            className="text-xs text-muted-foreground hover:underline"
          >
            Done
          </button>
        </div>
      )}

      <form onSubmit={handleCreate} className="flex flex-wrap items-center gap-2 max-w-2xl">
        <input
          value={name}
          onChange={e => setName(e.target.value)}
          placeholder="Token name (e.g. laptop, ci)"
          maxLength={100}
          aria-label="Token name"
          className="flex-1 min-w-48 px-3 py-2 border rounded-md bg-background text-sm focus:outline-none focus:ring-2 focus:ring-primary"
        />
        <Select
          value={lifetime}
          onChange={value => {
            if (isTokenLifetime(value)) setLifetime(value)
          }}
          options={lifetimeOptions}
          className="w-32"
        />
        <button
          type="submit"
          disabled={creating || !name.trim()}
          className="flex items-center gap-1.5 px-3 py-2 bg-primary text-primary-foreground rounded-md text-sm font-medium hover:bg-primary/90 disabled:opacity-50"
        >
          <Plus className="h-4 w-4" /> {creating ? 'Creating...' : 'Create token'}
        </button>
      </form>

      {loadError ? (
        <div className="text-sm text-destructive">{errorMessage(loadError, 'Failed to load tokens')}</div>
      ) : isLoading ? (
        <div className="text-sm text-muted-foreground">Loading tokens...</div>
      ) : tokens.length === 0 ? (
        <div className="text-sm text-muted-foreground">No tokens yet.</div>
      ) : (
        <div className="border rounded-md overflow-x-auto">
          <table className="w-full text-sm">
            <thead className="bg-muted/50">
              <tr>
                <th className="text-left p-2 font-medium">Name</th>
                <th className="text-left p-2 font-medium">Prefix</th>
                <th className="text-left p-2 font-medium">Created</th>
                <th className="text-left p-2 font-medium">Last used</th>
                <th className="text-left p-2 font-medium">Expires</th>
                <th className="text-right p-2 font-medium">Actions</th>
              </tr>
            </thead>
            <tbody>
              {tokens.map(token => (
                <tr key={token.id} className="border-t">
                  <td className="p-2">{token.name}</td>
                  <td className="p-2 font-mono text-xs text-muted-foreground">{token.token_prefix}…</td>
                  <td className="p-2 text-muted-foreground">
                    {parseUTCDate(token.created_at).toLocaleDateString()}
                  </td>
                  <td className="p-2 text-muted-foreground">
                    {token.last_used_at ? <TimeAgo date={token.last_used_at} /> : 'never'}
                  </td>
                  <td className="p-2 text-muted-foreground">
                    {token.expires_at ? parseUTCDate(token.expires_at).toLocaleDateString() : 'never'}
                  </td>
                  <td className="p-2 text-right">
                    <button
                      type="button"
                      onClick={() => handleRevoke(token)}
                      className="p-1.5 rounded hover:bg-destructive/10 text-destructive"
                      title="Revoke"
                      aria-label={`Revoke ${token.name}`}
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
