import { useState, useEffect } from 'react'
import { useRouter } from '../../router'
import { useAuth } from '../../lib/auth'
import { getInitials } from '../../lib/formatters'
import { prefetchRouteData } from '../../lib/route-prefetch'
import {
  BarChart3, AlertTriangle, MessageSquare, Shield, FlaskConical,
  FolderGit2, Brain, ScrollText, LayoutDashboard, ListChecks, GitPullRequest,
  Users, LogOut, Gauge, Settings, Sun, Moon, Ticket, GraduationCap, BookOpen,
  MessageSquareCode, Package, Hash,
} from 'lucide-react'
import { setSentryColorScheme } from '../../lib/sentry'

type NavGroup = {
  label?: string
  items: { path: string; label: string; icon: typeof LayoutDashboard }[]
}

const navGroups: NavGroup[] = [
  {
    items: [
      { path: '/', label: 'Overview', icon: LayoutDashboard },
    ],
  },
  {
    label: 'Work',
    items: [
      { path: '/issues', label: 'Issues', icon: Ticket },
      { path: '/attempts', label: 'Attempts', icon: ListChecks },
      { path: '/prs', label: 'PRs', icon: GitPullRequest },
    ],
  },
  {
    label: 'Insights',
    items: [
      { path: '/analytics', label: 'Analytics', icon: BarChart3 },
      { path: '/errors', label: 'Errors', icon: AlertTriangle },
      { path: '/regressions', label: 'Regressions', icon: Shield },
      { path: '/feedback', label: 'Feedback', icon: MessageSquare },
    ],
  },
  {
    label: 'Platform',
    items: [
      { path: '/experiments', label: 'Experiments', icon: FlaskConical },
      { path: '/inference', label: 'Inference', icon: Brain },
      { path: '/repos', label: 'Repos', icon: FolderGit2 },
      { path: '/channels', label: 'Channels', icon: Hash },
      { path: '/learning', label: 'Learning', icon: GraduationCap },
    ],
  },
  {
    label: 'Tools',
    items: [
      { path: '/chat', label: 'Chat', icon: MessageSquareCode },
      { path: '/models', label: 'Models', icon: Package },
    ],
  },
  {
    label: 'Monitoring',
    items: [
      { path: '/activity', label: 'Activity', icon: ScrollText },
      { path: '/telemetry', label: 'Telemetry', icon: Gauge },
    ],
  },
]

export function Sidebar() {
  const { path, navigate } = useRouter()
  const { user, logout } = useAuth()
  const [dark, setDark] = useState(() => document.documentElement.classList.contains('dark'))

  const handlePrefetch = (nextPath: string) => {
    prefetchRouteData(nextPath)
  }

  useEffect(() => {
    document.documentElement.classList.toggle('dark', dark)
    localStorage.setItem('theme', dark ? 'dark' : 'light')
    setSentryColorScheme(dark)
  }, [dark])

  return (
    <aside className="w-56 border-r bg-card flex flex-col h-screen sticky top-0">
      <div className="p-4 border-b">
        <h1 className="text-xl font-bold flex items-center gap-0 font-mono tracking-tight">
          claudear
          <span className="inline-block w-[10px] h-[20px] bg-primary ml-0.5 rounded-[1px]" />
        </h1>
      </div>
      <nav className="flex-1 p-2 overflow-y-auto">
        {navGroups.map((group, gi) => (
          <div key={gi} className={gi > 0 ? 'mt-3' : ''}>
            {group.label && (
              <div className="mb-1 px-3 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground/60">
                {group.label}
              </div>
            )}
            <div className="space-y-0.5">
              {group.items.map(({ path: itemPath, label, icon: Icon }) => {
                const isActive = path === itemPath
                return (
                  <button
                    key={itemPath}
                    onClick={() => navigate(itemPath)}
                    onMouseEnter={() => handlePrefetch(itemPath)}
                    onFocus={() => handlePrefetch(itemPath)}
                    className={`w-full flex items-center gap-2 px-3 py-2 rounded-md text-sm transition-colors ${
                      isActive
                        ? 'bg-primary/10 text-primary font-medium'
                        : 'text-muted-foreground hover:bg-muted hover:text-foreground'
                    }`}
                  >
                    <Icon className="h-4 w-4 shrink-0" />
                    {label}
                  </button>
                )
              })}
            </div>
          </div>
        ))}
        {user?.role === 'admin' && (
          <div className="mt-3">
            <div className="mb-1 px-3 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground/60">
              Admin
            </div>
            <div className="space-y-0.5">
              <button
                onClick={() => navigate('/config')}
                onMouseEnter={() => handlePrefetch('/config')}
                onFocus={() => handlePrefetch('/config')}
                className={`w-full flex items-center gap-2 px-3 py-2 rounded-md text-sm transition-colors ${
                  path === '/config'
                    ? 'bg-primary/10 text-primary font-medium'
                    : 'text-muted-foreground hover:bg-muted hover:text-foreground'
                }`}
              >
                <Settings className="h-4 w-4 shrink-0" />
                Config
              </button>
              <button
                onClick={() => navigate('/users')}
                onMouseEnter={() => handlePrefetch('/users')}
                onFocus={() => handlePrefetch('/users')}
                className={`w-full flex items-center gap-2 px-3 py-2 rounded-md text-sm transition-colors ${
                  path === '/users'
                    ? 'bg-primary/10 text-primary font-medium'
                    : 'text-muted-foreground hover:bg-muted hover:text-foreground'
                }`}
              >
                <Users className="h-4 w-4 shrink-0" />
                Users
              </button>
            </div>
          </div>
        )}
      </nav>
      <div className="p-3 border-t">
        <div className="flex items-center justify-between">
          <button
            onClick={() => navigate('/settings')}
            onMouseEnter={() => handlePrefetch('/settings')}
            onFocus={() => handlePrefetch('/settings')}
            className="flex items-center gap-2 min-w-0 hover:opacity-80 transition-opacity"
            title="Account settings"
          >
            {user?.avatar_url ? (
              <img src={user.avatar_url} alt={user.name} className="h-8 w-8 rounded-full object-cover shrink-0" />
            ) : (
              <div className="h-8 w-8 rounded-full bg-primary/10 text-primary flex items-center justify-center text-xs font-semibold shrink-0">
                {getInitials(user?.name ?? '')}
              </div>
            )}
            <div className="min-w-0 text-left">
              <div className="text-sm font-medium truncate">{user?.name}</div>
              <div className="text-xs text-muted-foreground truncate">{user?.email}</div>
            </div>
          </button>
          <div className="flex items-center gap-0.5">
            <a
              href="/docs/"
              target="_blank"
              rel="noopener noreferrer"
              className="p-1.5 rounded hover:bg-muted text-muted-foreground"
              title="Documentation"
            >
              <BookOpen className="h-4 w-4" />
            </a>
            <button
              onClick={() => setDark(d => !d)}
              className="p-1.5 rounded hover:bg-muted text-muted-foreground"
              title={dark ? 'Switch to light mode' : 'Switch to dark mode'}
            >
              {dark ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
            </button>
            <button
              onClick={logout}
              className="p-1.5 rounded hover:bg-muted text-muted-foreground"
              title="Sign out"
            >
              <LogOut className="h-4 w-4" />
            </button>
          </div>
        </div>
      </div>
    </aside>
  )
}
