import useSWR from 'swr'
import { ArrowRight, KeyRound, Pencil } from 'lucide-react'
import {
  ApiError,
  fetchFeatures,
  type FeatureCategory,
  type FeatureSecret,
  type FeatureSetting,
  type FeatureSettingValue,
  type FeatureStatus,
} from '../lib/api'
import { useAuth } from '../lib/auth'
import { useRouter } from '../router'
import { PageHeader } from '../components/layout/page-header'
import { CardStackSkeleton } from '../components/shared/page-skeletons'
import { EmptyState } from '../components/shared/empty-state'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '../components/ui/card'

const FEATURES_KEY = 'features'
const ADMIN_ROLE = 'admin'
const CONFIG_PATH = '/config'
const LOAD_FAILURE_MESSAGE = 'Failed to load integrations.'
const UNKNOWN_CATEGORY_LABEL = 'Other'

interface CategoryMeta {
  label: string
  description: string
}

const CATEGORIES: Record<FeatureCategory, CategoryMeta> = {
  source: { label: 'Issue sources', description: 'Where claudear picks up work' },
  scm: { label: 'Source control', description: 'Where claudear opens pull requests' },
  notifier: { label: 'Notifiers', description: 'Where claudear reports progress' },
  automation: { label: 'Automation', description: 'How attempts are run, retried and followed up' },
  intelligence: { label: 'Intelligence', description: 'Models, indexing and learning' },
  platform: { label: 'Platform', description: 'Dashboard, transport and the MCP server' },
}

const CATEGORY_ORDER = Object.keys(CATEGORIES) as FeatureCategory[]

type FeatureState = 'enabled' | 'disabled' | 'not_configured'

const FEATURE_STATES: Record<FeatureState, { label: string; className: string }> = {
  enabled: {
    label: 'Enabled',
    className: 'bg-green-500/10 text-green-700 dark:text-green-400',
  },
  disabled: {
    label: 'Disabled',
    className: 'bg-amber-500/10 text-amber-700 dark:text-amber-400',
  },
  not_configured: {
    label: 'Not configured',
    className: 'bg-zinc-500/10 text-zinc-600 dark:text-zinc-400',
  },
}

interface FeaturePage {
  path: string
  label: string
}

const FEATURE_PAGES: ReadonlyMap<string, FeaturePage> = new Map([
  ['deploy_qa', { path: '/deploy-qa', label: 'Deploy QA' }],
  ['regression', { path: '/regressions', label: 'Regressions' }],
  ['knowledgebase.discord', { path: '/channels', label: 'Channels' }],
  ['chat', { path: '/chat', label: 'Chat' }],
  ['llm', { path: '/models', label: 'Models' }],
  ['code_index', { path: '/repos', label: 'Repos' }],
  ['learning', { path: '/learning', label: 'Learning' }],
])

interface FeatureGroup {
  key: string
  label: string
  description: string | null
  features: FeatureStatus[]
}

const actionClassName =
  'inline-flex items-center gap-1.5 rounded-md border px-2.5 py-1 text-xs font-medium hover:bg-muted transition-colors cursor-pointer'

function isKnownCategory(category: string): category is FeatureCategory {
  return (CATEGORY_ORDER as string[]).includes(category)
}

function featureState(feature: FeatureStatus): FeatureState {
  if (!feature.configured) return 'not_configured'
  return feature.enabled ? 'enabled' : 'disabled'
}

function groupFeatures(features: FeatureStatus[]): FeatureGroup[] {
  const groups: FeatureGroup[] = CATEGORY_ORDER.map(category => ({
    key: category,
    label: CATEGORIES[category].label,
    description: CATEGORIES[category].description,
    features: features.filter(feature => feature.category === category),
  }))
  const uncategorised = features.filter(feature => !isKnownCategory(feature.category))
  groups.push({ key: 'other', label: UNKNOWN_CATEGORY_LABEL, description: null, features: uncategorised })
  return groups.filter(group => group.features.length > 0)
}

function formatSettingValue(value: FeatureSettingValue): string | null {
  if (value === null) return null
  if (typeof value === 'boolean') return value ? 'Yes' : 'No'
  if (typeof value === 'number') return String(value)
  if (typeof value === 'string') return value.trim() === '' ? null : value
  if (Array.isArray(value)) {
    const items = value.map(formatSettingValue).filter((item): item is string => item !== null)
    return items.length > 0 ? items.join(', ') : null
  }
  return JSON.stringify(value)
}

function describeLoadFailure(error: unknown): string {
  return error instanceof ApiError ? error.message : LOAD_FAILURE_MESSAGE
}

function StateBadge({ state }: { state: FeatureState }) {
  const { label, className } = FEATURE_STATES[state]
  return <span className={`shrink-0 px-2 py-1 rounded text-xs font-medium ${className}`}>{label}</span>
}

function SettingsList({ settings }: { settings: FeatureSetting[] }) {
  if (settings.length === 0) return null
  return (
    <dl className="grid grid-cols-[minmax(0,auto)_minmax(0,1fr)] gap-x-4 gap-y-1 text-xs">
      {settings.map(setting => {
        const formatted = formatSettingValue(setting.value)
        return (
          <div key={setting.name} className="contents">
            <dt className="font-mono text-muted-foreground truncate">{setting.name}</dt>
            <dd className="font-mono break-words">
              {formatted ?? <span className="text-muted-foreground">--</span>}
            </dd>
          </div>
        )
      })}
    </dl>
  )
}

function SecretChips({ secrets }: { secrets: FeatureSecret[] }) {
  if (secrets.length === 0) return null
  return (
    <ul aria-label="Secrets" className="flex flex-wrap gap-1.5">
      {secrets.map(secret => (
        <li
          key={secret.name}
          className={`inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-xs ${
            secret.set ? 'border-green-500/30 text-green-700 dark:text-green-400' : 'text-muted-foreground'
          }`}
        >
          <KeyRound className="h-3 w-3" />
          <span className="font-mono">{secret.name}</span>
          <span>{secret.set ? 'set' : 'not set'}</span>
        </li>
      ))}
    </ul>
  )
}

function FeatureCard({ feature, isAdmin }: { feature: FeatureStatus; isAdmin: boolean }) {
  const { navigate } = useRouter()
  const page = FEATURE_PAGES.get(feature.key)

  return (
    <Card aria-labelledby={`feature-${feature.key}`} role="article" className="flex flex-col">
      <CardHeader className="pb-3">
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0 space-y-1">
            <CardTitle id={`feature-${feature.key}`} className="text-base">
              {feature.label}
            </CardTitle>
            <p className="font-mono text-xs text-muted-foreground">{feature.key}</p>
          </div>
          <StateBadge state={featureState(feature)} />
        </div>
        <CardDescription>{feature.description}</CardDescription>
      </CardHeader>
      <CardContent className="flex flex-1 flex-col gap-3">
        <SettingsList settings={feature.settings} />
        <SecretChips secrets={feature.secrets} />
        {(page || isAdmin) && (
          <div className="mt-auto flex flex-wrap gap-2 pt-1">
            {page && (
              <button type="button" className={actionClassName} onClick={() => navigate(page.path)}>
                Open {page.label}
                <ArrowRight className="h-3 w-3" />
              </button>
            )}
            {isAdmin && (
              <button type="button" className={actionClassName} onClick={() => navigate(CONFIG_PATH)}>
                <Pencil className="h-3 w-3" />
                Edit in Config
              </button>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  )
}

function FeatureGroupSection({ group, isAdmin }: { group: FeatureGroup; isAdmin: boolean }) {
  const headingId = `category-${group.key}`
  const enabledCount = group.features.filter(feature => featureState(feature) === 'enabled').length

  return (
    <section aria-labelledby={headingId} className="space-y-3">
      <div>
        <h3 id={headingId} className="text-lg font-semibold">
          {group.label}
        </h3>
        <p className="text-sm text-muted-foreground">
          {group.description && `${group.description}. `}
          {enabledCount} of {group.features.length} enabled
        </p>
      </div>
      <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
        {group.features.map(feature => (
          <FeatureCard key={feature.key} feature={feature} isAdmin={isAdmin} />
        ))}
      </div>
    </section>
  )
}

export default function IntegrationsPage() {
  const { user } = useAuth()
  const isAdmin = user?.role === ADMIN_ROLE
  const { data, error, isLoading } = useSWR<FeatureStatus[]>(FEATURES_KEY, fetchFeatures)

  return (
    <div className="space-y-6">
      <PageHeader
        title="Integrations"
        description="Shows the configuration claudear is running with. Changes take effect after restart."
      />

      {error ? (
        <div role="alert" className="rounded-md bg-destructive/10 text-destructive text-sm p-3">
          {describeLoadFailure(error)}
        </div>
      ) : isLoading || !data ? (
        <CardStackSkeleton />
      ) : data.length === 0 ? (
        <EmptyState message="No integrations reported" />
      ) : (
        groupFeatures(data).map(group => <FeatureGroupSection key={group.key} group={group} isAdmin={isAdmin} />)
      )}
    </div>
  )
}
