import { useEffect, useState } from 'react'
import { ApiError, acceptInvite, inspectInvite, type InviteDetails } from '../lib/api'
import { formatDate } from '../lib/formatters'
import { clearInviteToken, readInviteToken } from '../lib/invite-token'
import { PASSWORD_MIN_BYTES, passwordLengthProblem } from '../lib/password'
import { hideSentryFeedbackWidget } from '../lib/sentry'
import {
  AUTH_ALERT_CLASS,
  AUTH_BUTTON_CLASS,
  AUTH_INPUT_CLASS,
  AUTH_LABEL_CLASS,
  AuthButtonLabel,
  AuthShell,
} from '../components/layout/auth-shell'

const NOT_FOUND = 404

type InviteView =
  | { kind: 'checking' }
  | { kind: 'invalid' }
  | { kind: 'unavailable' }
  | { kind: 'ready'; details: InviteDetails }
  | { kind: 'accepted'; details: InviteDetails }

function viewForFailure(error: unknown): InviteView {
  return error instanceof ApiError && error.status === NOT_FOUND ? { kind: 'invalid' } : { kind: 'unavailable' }
}

export function passwordProblem(password: string, confirmation: string): string | null {
  const lengthProblem = passwordLengthProblem(password)
  if (lengthProblem) return lengthProblem
  if (password !== confirmation) return 'Passwords do not match'
  return null
}

export default function InvitePage() {
  const [token] = useState(readInviteToken)
  const [view, setView] = useState<InviteView>(() => (token ? { kind: 'checking' } : { kind: 'invalid' }))

  useEffect(() => {
    hideSentryFeedbackWidget()
    const reloadForNewLink = () => { if (window.location.hash) window.location.reload() }
    window.addEventListener('hashchange', reloadForNewLink)
    return () => window.removeEventListener('hashchange', reloadForNewLink)
  }, [])

  useEffect(() => {
    if (!token) return
    let cancelled = false
    inspectInvite(token)
      .then((details) => { if (!cancelled) setView({ kind: 'ready', details }) })
      .catch((error: unknown) => { if (!cancelled) setView(viewForFailure(error)) })
    return () => { cancelled = true }
  }, [token])

  return (
    <>
      <title>Accept invite — Claudear</title>
      {view.kind === 'checking' && (
        <AuthShell title="Accept invite" subtitle="Checking your invite link…">
          <div className="text-sm text-muted-foreground">Loading...</div>
        </AuthShell>
      )}
      {view.kind === 'invalid' && (
        <InviteProblem message="This invite link is invalid or has expired. Ask an administrator for a new one." />
      )}
      {view.kind === 'unavailable' && (
        <InviteProblem message="We couldn't check this invite link right now. Try again in a moment." retry />
      )}
      {view.kind === 'ready' && token && (
        <SetPasswordForm
          token={token}
          details={view.details}
          onAccepted={() => setView({ kind: 'accepted', details: view.details })}
          onFailure={(error) => setView(viewForFailure(error))}
        />
      )}
      {view.kind === 'accepted' && (
        <AuthShell title="You're all set" subtitle={`Your password is saved. Sign in as ${view.details.email}.`}>
          <a href="/" className={AUTH_BUTTON_CLASS}>
            <AuthButtonLabel>Sign in</AuthButtonLabel>
          </a>
        </AuthShell>
      )}
    </>
  )
}

function InviteProblem({ message, retry = false }: { message: string; retry?: boolean }) {
  return (
    <AuthShell title="Invite unavailable" subtitle="This link can't be used to set up an account.">
      <div className="space-y-4">
        <div role="alert" className={AUTH_ALERT_CLASS}>{message}</div>
        {retry ? (
          <button type="button" onClick={() => window.location.reload()} className={AUTH_BUTTON_CLASS}>
            <AuthButtonLabel>Try again</AuthButtonLabel>
          </button>
        ) : (
          <a href="/" className={AUTH_BUTTON_CLASS}>
            <AuthButtonLabel>Go to sign in</AuthButtonLabel>
          </a>
        )}
      </div>
    </AuthShell>
  )
}

function SetPasswordForm({
  token,
  details,
  onAccepted,
  onFailure,
}: {
  token: string
  details: InviteDetails
  onAccepted: () => void
  onFailure: (error: unknown) => void
}) {
  const [password, setPassword] = useState('')
  const [confirmation, setConfirmation] = useState('')
  const [error, setError] = useState('')
  const [saving, setSaving] = useState(false)

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    const problem = passwordProblem(password, confirmation)
    if (problem) {
      setError(problem)
      return
    }
    setError('')
    setSaving(true)
    try {
      await acceptInvite(token, password)
      clearInviteToken()
      onAccepted()
    } catch (failure: unknown) {
      if (!(failure instanceof ApiError) || failure.status >= 500) {
        setError("Couldn't save your password. Try again in a moment.")
      } else if (failure.status === NOT_FOUND) {
        onFailure(failure)
      } else {
        setError(failure.message)
      }
    } finally {
      setSaving(false)
    }
  }

  return (
    <AuthShell
      title={`Welcome, ${details.name}`}
      subtitle={`Set a password for ${details.email} to finish setting up your account.`}
    >
      <form onSubmit={handleSubmit} className="space-y-4">
        {error && <div role="alert" className={AUTH_ALERT_CLASS}>{error}</div>}

        <input type="email" value={details.email} autoComplete="username" readOnly hidden />

        <div className="space-y-2">
          <label htmlFor="invite-password" className={AUTH_LABEL_CLASS}>Password</label>
          <input
            id="invite-password"
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            required
            autoFocus
            autoComplete="new-password"
            className={AUTH_INPUT_CLASS}
            placeholder={`At least ${PASSWORD_MIN_BYTES} bytes`}
          />
        </div>

        <div className="space-y-2">
          <label htmlFor="invite-confirmation" className={AUTH_LABEL_CLASS}>Confirm password</label>
          <input
            id="invite-confirmation"
            type="password"
            value={confirmation}
            onChange={(e) => setConfirmation(e.target.value)}
            required
            autoComplete="new-password"
            className={AUTH_INPUT_CLASS}
            placeholder="Repeat your password"
          />
        </div>

        <p className="text-xs text-muted-foreground">
          This link works once and expires {formatDate(details.expires_at)}.
        </p>

        <button type="submit" disabled={saving} className={AUTH_BUTTON_CLASS}>
          <AuthButtonLabel>{saving ? 'Saving...' : 'Set password'}</AuthButtonLabel>
        </button>
      </form>
    </AuthShell>
  )
}
