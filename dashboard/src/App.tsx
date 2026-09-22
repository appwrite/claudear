import { useEffect, type JSX } from 'react'
import { Router, RouterProvider } from './router'
import { AppShell } from './components/layout/app-shell'
import { AuthProvider, useAuth } from './lib/auth'
import { Sentry } from './lib/sentry'
import LoginPage from './pages/login'
import OverviewPage from './pages/overview'
import AttemptsPage from './pages/attempts'
import PrsPage from './pages/prs'
import AnalyticsPage from './pages/analytics'
import ErrorsPage from './pages/errors'
import FeedbackPage from './pages/feedback'
import RegressionsPage from './pages/regressions'
import ExperimentsPage from './pages/experiments'
import ReposPage from './pages/repos'
import ChannelsPage from './pages/channels'
import InferencePage from './pages/inference'
import ActivityPage from './pages/activity'
import UsersPage from './pages/users'
import TelemetryPage from './pages/telemetry'
import ConfigPage from './pages/config'
import IssuesPage from './pages/issues'
import SettingsPage from './pages/settings'
import LearningPage from './pages/learning'
import ChatPage from './pages/chat'
import ModelsPage from './pages/models'

function SentryFallback() {
  return (
    <div className="min-h-screen bg-background flex items-center justify-center">
      <div className="text-center space-y-4 max-w-md px-4">
        <h1 className="text-xl font-semibold text-foreground">Something went wrong</h1>
        <p className="text-sm text-muted-foreground">
          An unexpected error occurred. The error has been reported automatically.
        </p>
        <button
          onClick={() => window.location.reload()}
          className="inline-flex items-center justify-center rounded-md text-sm font-medium bg-primary text-primary-foreground h-9 px-4 hover:bg-primary/90"
        >
          Reload page
        </button>
      </div>
    </div>
  )
}

const routes: Record<string, () => JSX.Element | null> = {
  '/': OverviewPage,
  '/issues': IssuesPage,
  '/attempts': AttemptsPage,
  '/prs': PrsPage,
  '/analytics': AnalyticsPage,
  '/errors': ErrorsPage,
  '/feedback': FeedbackPage,
  '/regressions': RegressionsPage,
  '/experiments': ExperimentsPage,
  '/repos': ReposPage,
  '/channels': ChannelsPage,
  '/learning': LearningPage,
  '/chat': ChatPage,
  '/models': ModelsPage,
  '/inference': InferencePage,
  '/activity': ActivityPage,
  '/telemetry': TelemetryPage,
  '/config': ConfigPage,
  '/users': UsersPage,
  '/settings': SettingsPage,
}

function AuthenticatedApp() {
  const { user, loading } = useAuth()

  useEffect(() => {
    if (user) Sentry.setTag('user.role', user.role)
  }, [user])

  return (
    <Sentry.ErrorBoundary fallback={<SentryFallback />}>
      {loading ? (
        <div className="min-h-screen bg-background flex items-center justify-center">
          <div className="text-muted-foreground text-sm">Loading...</div>
        </div>
      ) : !user ? (
        <LoginPage />
      ) : (
        <RouterProvider>
          <AppShell>
            <Router routes={routes} />
          </AppShell>
        </RouterProvider>
      )}
    </Sentry.ErrorBoundary>
  )
}

function App() {
  return (
    <AuthProvider>
      <AuthenticatedApp />
    </AuthProvider>
  )
}

export default App
