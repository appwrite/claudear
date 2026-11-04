import { useEffect, useState } from 'react'
import { useAuth } from '../lib/auth'
import { hideSentryFeedbackWidget } from '../lib/sentry'
import {
  AUTH_ALERT_CLASS,
  AUTH_BUTTON_CLASS,
  AUTH_INPUT_CLASS,
  AUTH_LABEL_CLASS,
  AuthButtonLabel,
  AuthShell,
} from '../components/layout/auth-shell'

export default function LoginPage() {
  const { login } = useAuth()
  const [email, setEmail] = useState('')
  const [password, setPassword] = useState('')
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault()
    setError('')
    setLoading(true)
    try {
      await login(email, password)
    } catch {
      setError('Invalid email or password')
    } finally {
      setLoading(false)
    }
  }

  useEffect(() => {
    hideSentryFeedbackWidget()
  }, [])

  return (
    <AuthShell title="Sign in" subtitle="Dashboard access">
      <title>Login — Claudear</title>
      <form onSubmit={handleSubmit} className="space-y-4">
        {error && (
          <div role="alert" className={AUTH_ALERT_CLASS}>
            {error}
          </div>
        )}

        <div className="space-y-2">
          <label htmlFor="email" className={AUTH_LABEL_CLASS}>
            Email
          </label>
          <input
            id="email"
            type="email"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            required
            autoFocus
            autoComplete="email"
            className={AUTH_INPUT_CLASS}
            placeholder="admin@example.com"
          />
        </div>

        <div className="space-y-2">
          <label htmlFor="password" className={AUTH_LABEL_CLASS}>
            Password
          </label>
          <input
            id="password"
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            required
            autoComplete="current-password"
            className={AUTH_INPUT_CLASS}
            placeholder="Enter your password"
          />
        </div>

        <button type="submit" disabled={loading} className={AUTH_BUTTON_CLASS}>
          <AuthButtonLabel>{loading ? 'Signing in...' : 'Sign in'}</AuthButtonLabel>
        </button>
      </form>
    </AuthShell>
  )
}
