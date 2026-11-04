import { useRef, useState, type FormEvent, type ReactNode } from 'react'
import useSWR, { useSWRConfig } from 'swr'
import { AlertTriangle, CheckCircle2, Info, Loader2, Plus, RotateCcw, Save, Trash2 } from 'lucide-react'
import {
  fetchDeployQaConfig,
  fetchDeployQaStatus,
  fetchDeployQaTips,
  saveDeployQaConfig,
  type DeployQaConfigResponse,
  type DeployQaSaveResponse,
  type DeployQaSettings,
  type DeployQaStatus,
  type DeployQaTagFilterKind,
  type DeployQaTipView,
  type DeployQaTrackStatus,
} from '../lib/api'
import {
  composeTagFilter,
  findTagFilterKind,
  formatDuration,
  isExternalUrl,
  parseTagFilter,
  validateDeployQaSettings,
  type ValidationIssue,
} from '../lib/deploy-qa'
import { useAuth } from '../lib/auth'
import { useRouter } from '../router'
import { PageHeader } from '../components/layout/page-header'
import { BlockSkeleton, TableRowsSkeleton } from '../components/shared/page-skeletons'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '../components/ui/card'
import { Badge } from '../components/ui/badge'
import { Select } from '../components/ui/select'
import { DataTable, type Column } from '../components/shared/data-table'
import { StatusBadge } from '../components/shared/status-badge'
import { TimeAgo } from '../components/shared/time-ago'

const STATUS_KEY = 'deploy-qa-status'
const TIPS_KEY = 'deploy-qa-tips'
const CONFIG_KEY = 'deploy-qa-config'
const ADMIN_ROLE = 'admin'
const REVISION_MISMATCH_CODE = 'revision_mismatch'
const REFRESH_INTERVAL_MS = 30000

const SAVE_EXPLAINER =
  'Settings are saved to claudear.toml and take effect after claudear restarts. Saving rewrites the file in normalised form (comments removed).'

const inputClassName =
  'w-full px-3 py-1.5 text-sm border rounded-md bg-background focus:outline-none focus:ring-2 focus:ring-primary/30'
const linkClassName = 'text-primary hover:underline'

interface TrackDraft {
  id: number
  name: string
  repo: string
  kind: string
  value: string
}

interface SettingsDraft {
  enabled: boolean
  poll_interval_ms: string
  timeout_secs: string
  skip_if_previous_running: boolean
  discord_channel_id: string
  github_discord_map_path: string
  instructions_path: string
  tracks: TrackDraft[]
}

interface SaveFailure {
  message: string
  code?: string
}

function toDraft(config: DeployQaConfigResponse): SettingsDraft {
  const { settings, tag_filter_kinds: kinds } = config
  return {
    enabled: settings.enabled,
    poll_interval_ms: String(settings.poll_interval_ms),
    timeout_secs: String(settings.timeout_secs),
    skip_if_previous_running: settings.skip_if_previous_running,
    discord_channel_id: settings.discord_channel_id ?? '',
    github_discord_map_path: settings.github_discord_map_path ?? '',
    instructions_path: settings.instructions_path ?? '',
    tracks: settings.tracks.map((track, index) => ({
      id: index,
      name: track.name,
      repo: track.repo,
      ...parseTagFilter(track.tag_filter, kinds),
    })),
  }
}

function parseWholeNumber(text: string): number {
  return text.trim() === '' ? Number.NaN : Number(text)
}

function optionalText(text: string): string | null {
  const trimmed = text.trim()
  return trimmed === '' ? null : trimmed
}

function toSettings(draft: SettingsDraft, kinds: DeployQaTagFilterKind[]): DeployQaSettings {
  return {
    enabled: draft.enabled,
    poll_interval_ms: parseWholeNumber(draft.poll_interval_ms),
    skip_if_previous_running: draft.skip_if_previous_running,
    timeout_secs: parseWholeNumber(draft.timeout_secs),
    discord_channel_id: optionalText(draft.discord_channel_id),
    github_discord_map_path: optionalText(draft.github_discord_map_path),
    instructions_path: optionalText(draft.instructions_path),
    tracks: draft.tracks.map(track => {
      const kind = findTagFilterKind(track.kind, kinds)
      return {
        name: track.name.trim(),
        repo: track.repo.trim(),
        tag_filter: kind ? composeTagFilter(kind, track.value) : track.kind,
      }
    }),
  }
}

function describeSaveFailure(error: unknown): SaveFailure {
  const message = error instanceof Error ? error.message : String(error)
  const code = typeof error === 'object' && error !== null ? (error as { code?: unknown }).code : undefined
  return { message, code: typeof code === 'string' ? code : undefined }
}

function ExternalLink({ href, children }: { href: string | null; children: ReactNode }) {
  if (!isExternalUrl(href)) return null
  return (
    <a href={href} target="_blank" rel="noopener noreferrer" className={linkClassName}>
      {children}
    </a>
  )
}

function Placeholder() {
  return <span className="text-muted-foreground">--</span>
}

function Toggle({
  label,
  checked,
  onChange,
}: {
  label: string
  checked: boolean
  onChange: (checked: boolean) => void
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      onClick={() => onChange(!checked)}
      className={`relative w-10 h-5 shrink-0 rounded-full transition-colors cursor-pointer disabled:cursor-not-allowed ${
        checked ? 'bg-primary' : 'bg-muted-foreground/30'
      }`}
    >
      <span
        className={`absolute top-0.5 left-0.5 w-4 h-4 rounded-full bg-white transition-transform ${
          checked ? 'translate-x-5' : ''
        }`}
      />
    </button>
  )
}

function FieldIssue({ message }: { message?: string }) {
  if (!message) return null
  return <p className="text-xs text-destructive">{message}</p>
}

function StatusItem({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="space-y-1">
      <p className="text-xs uppercase tracking-wide text-muted-foreground">{label}</p>
      <div className="text-sm font-medium">{children}</div>
    </div>
  )
}

function StatusCard({ status }: { status: DeployQaStatus }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-lg">Status</CardTitle>
        <CardDescription>The deploy QA settings claudear is running with.</CardDescription>
      </CardHeader>
      <CardContent>
        <div className="grid grid-cols-2 md:grid-cols-5 gap-4">
          <StatusItem label="Enabled">
            <Badge variant={status.enabled ? 'default' : 'outline'}>{status.enabled ? 'Enabled' : 'Disabled'}</Badge>
          </StatusItem>
          <StatusItem label="Poll interval">{formatDuration(status.poll_interval_ms / 1000)}</StatusItem>
          <StatusItem label="Timeout">{formatDuration(status.timeout_secs)}</StatusItem>
          <StatusItem label="Skip if previous running">{status.skip_if_previous_running ? 'Yes' : 'No'}</StatusItem>
          <StatusItem label="Discord channel">
            {status.discord_channel_id ? <span className="font-mono">{status.discord_channel_id}</span> : <Placeholder />}
          </StatusItem>
        </div>
      </CardContent>
    </Card>
  )
}

const trackColumns: Column<DeployQaTrackStatus>[] = [
  {
    key: 'name',
    header: 'Track',
    render: track => <span className="font-mono whitespace-nowrap">{track.name}</span>,
  },
  {
    key: 'repo',
    header: 'Repository',
    render: track => (
      <a
        href={`https://github.com/${track.repo}/releases`}
        target="_blank"
        rel="noopener noreferrer"
        className={linkClassName}
      >
        {track.repo}
      </a>
    ),
  },
  {
    key: 'tag_filter',
    header: 'Tag filter',
    render: track => <span className="font-mono text-xs whitespace-nowrap">{track.tag_filter}</span>,
  },
  {
    key: 'last_seen',
    header: 'Last seen',
    render: track =>
      track.last_seen ? (
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-mono">{track.last_seen.tag}</span>
          <StatusBadge status={track.last_seen.status} />
          <TimeAgo date={track.last_seen.published_at ?? track.last_seen.updated_at} />
        </div>
      ) : (
        <Placeholder />
      ),
  },
  {
    key: 'in_flight',
    header: 'In flight',
    render: track =>
      track.in_flight ? (
        <span className="inline-flex items-center gap-1 text-xs text-blue-600 dark:text-blue-400">
          <Loader2 className="h-3 w-3 animate-spin" />
          In flight
        </span>
      ) : (
        <Placeholder />
      ),
  },
]

function TracksCard({ tracks }: { tracks: DeployQaTrackStatus[] }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-lg">Tracks</CardTitle>
        <CardDescription>Repositories whose new release tags get live QA.</CardDescription>
      </CardHeader>
      <CardContent>
        <DataTable
          columns={trackColumns}
          data={tracks}
          keyFn={track => track.name}
          emptyMessage="No deploy QA tracks configured"
          pageSize={0}
        />
      </CardContent>
    </Card>
  )
}

function RecentRunsCard({
  tracks,
  track,
  onTrackChange,
}: {
  tracks: DeployQaTrackStatus[]
  track: string
  onTrackChange: (track: string) => void
}) {
  const { navigate } = useRouter()
  const { data, error, isLoading } = useSWR<DeployQaTipView[]>(
    [TIPS_KEY, track],
    () => fetchDeployQaTips(track),
    { refreshInterval: REFRESH_INTERVAL_MS },
  )

  const columns: Column<DeployQaTipView>[] = [
    {
      key: 'tag',
      header: 'Tag',
      render: tip => <span className="font-mono">{tip.tag}</span>,
    },
    {
      key: 'track',
      header: 'Track',
      render: tip => <span className="font-mono text-xs whitespace-nowrap">{tip.track}</span>,
    },
    {
      key: 'status',
      header: 'Status',
      render: tip => <StatusBadge status={tip.status} />,
    },
    {
      key: 'published_at',
      header: 'Published',
      render: tip => (tip.published_at ? <TimeAgo date={tip.published_at} /> : <Placeholder />),
    },
    {
      key: 'author_login',
      header: 'Author',
      render: tip => (tip.author_login ? <span className="text-sm">{tip.author_login}</span> : <Placeholder />),
    },
    {
      key: 'links',
      header: 'Links',
      render: tip => (
        <div className="flex flex-wrap gap-3 text-xs">
          <ExternalLink href={tip.release_url}>Release</ExternalLink>
          {tip.attempt_id !== null && (
            <button
              type="button"
              onClick={() => navigate(`/attempts?id=${tip.attempt_id}`)}
              className={linkClassName}
            >
              Run log
            </button>
          )}
          <ExternalLink href={tip.discord_message_url}>Discord message</ExternalLink>
          <ExternalLink href={tip.discord_thread_url}>Discord thread</ExternalLink>
        </div>
      ),
    },
  ]

  return (
    <Card>
      <CardHeader>
        <div className="flex flex-col gap-3 md:flex-row md:items-center md:justify-between">
          <div className="space-y-1.5">
            <CardTitle className="text-lg">Recent runs</CardTitle>
            <CardDescription>Release tags picked up by deploy QA, newest first.</CardDescription>
          </div>
          <label className="md:w-56">
            <span className="sr-only">Filter runs by track</span>
            <Select
              value={track}
              onChange={onTrackChange}
              placeholder="All tracks"
              options={tracks.map(item => ({ value: item.name, label: item.name }))}
            />
          </label>
        </div>
      </CardHeader>
      <CardContent>
        {error ? (
          <div className="text-destructive text-sm">Failed to load deploy QA runs.</div>
        ) : isLoading ? (
          <TableRowsSkeleton rows={4} />
        ) : (
          <DataTable
            columns={columns}
            data={data ?? []}
            keyFn={tip => tip.id}
            emptyMessage="No deploy QA runs yet"
          />
        )}
      </CardContent>
    </Card>
  )
}

function TrackEditor({
  index,
  track,
  kinds,
  issueFor,
  onChange,
  onRemove,
}: {
  index: number
  track: TrackDraft
  kinds: DeployQaTagFilterKind[]
  issueFor: (path: string) => string | undefined
  onChange: (update: Partial<TrackDraft>) => void
  onRemove: () => void
}) {
  const number = index + 1
  const kind = findTagFilterKind(track.kind, kinds)
  const kindOptions = kinds.map(item => ({ value: item.kind, label: item.kind }))
  if (!kind) {
    kindOptions.push({ value: track.kind, label: `${track.kind} (unknown)` })
  }

  return (
    <div className="rounded-md border p-3 space-y-3">
      <div className="flex items-center justify-between">
        <p className="text-sm font-medium">Track {number}</p>
        <button
          type="button"
          onClick={onRemove}
          aria-label={`Remove track ${number}`}
          className="inline-flex items-center gap-1 px-2 py-1 text-xs border rounded-md hover:bg-muted"
        >
          <Trash2 className="h-3 w-3" />
          Remove
        </button>
      </div>
      <div className="grid grid-cols-1 md:grid-cols-3 gap-3">
        <div className="space-y-1">
          <label className="space-y-1 block">
            <span className="text-xs font-medium">Name</span>
            <input
              aria-label={`Track ${number} name`}
              value={track.name}
              onChange={event => onChange({ name: event.target.value })}
              placeholder="cloud"
              className={inputClassName}
            />
          </label>
          <FieldIssue message={issueFor(`tracks[${index}].name`)} />
        </div>
        <div className="space-y-1">
          <label className="space-y-1 block">
            <span className="text-xs font-medium">Repository</span>
            <input
              aria-label={`Track ${number} repository`}
              value={track.repo}
              onChange={event => onChange({ repo: event.target.value })}
              placeholder="owner/name"
              className={inputClassName}
            />
          </label>
          <FieldIssue message={issueFor(`tracks[${index}].repo`)} />
        </div>
        <div className="space-y-1">
          <span className="text-xs font-medium">Tag filter</span>
          <div className="flex gap-2">
            <label className="flex-1">
              <span className="sr-only">Track {number} tag filter kind</span>
              <Select value={track.kind} onChange={value => onChange({ kind: value })} options={kindOptions} />
            </label>
            {kind?.requires_value && (
              <input
                aria-label={`Track ${number} tag filter value`}
                value={track.value}
                onChange={event => onChange({ value: event.target.value })}
                placeholder="value"
                className={`${inputClassName} flex-1`}
              />
            )}
          </div>
          <FieldIssue message={issueFor(`tracks[${index}].tag_filter`)} />
        </div>
      </div>
    </div>
  )
}

function SettingsCard({
  config,
  onSaved,
  onReload,
}: {
  config: DeployQaConfigResponse
  onSaved: (response: DeployQaSaveResponse) => void
  onReload: () => void
}) {
  const kinds = config.tag_filter_kinds
  const [draft, setDraft] = useState<SettingsDraft>(() => toDraft(config))
  const [saving, setSaving] = useState(false)
  const [failure, setFailure] = useState<SaveFailure | null>(null)
  const nextTrackId = useRef(config.settings.tracks.length)

  const settings = toSettings(draft, kinds)
  const issues: ValidationIssue[] = validateDeployQaSettings(settings, kinds, config.track_name_separator)
  const issueFor = (path: string) => issues.find(issue => issue.path === path)?.message

  const update = (change: Partial<SettingsDraft>) => setDraft(current => ({ ...current, ...change }))

  const updateTrack = (id: number, change: Partial<TrackDraft>) =>
    setDraft(current => ({
      ...current,
      tracks: current.tracks.map(track => (track.id === id ? { ...track, ...change } : track)),
    }))

  const addTrack = () => {
    const id = nextTrackId.current
    nextTrackId.current += 1
    setDraft(current => ({
      ...current,
      tracks: [...current.tracks, { id, name: '', repo: '', kind: kinds[0]?.kind ?? '', value: '' }],
    }))
  }

  const removeTrack = (id: number) =>
    setDraft(current => ({ ...current, tracks: current.tracks.filter(track => track.id !== id) }))

  const handleSubmit = async (event: FormEvent) => {
    event.preventDefault()
    if (!config.writable || issues.length > 0) return
    setSaving(true)
    setFailure(null)
    try {
      const response = await saveDeployQaConfig(settings, config.revision)
      onSaved(response)
    } catch (error) {
      setFailure(describeSaveFailure(error))
    } finally {
      setSaving(false)
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-lg">Settings</CardTitle>
        <CardDescription className="flex items-start gap-2">
          <Info className="h-4 w-4 mt-0.5 shrink-0" />
          <span>{SAVE_EXPLAINER}</span>
        </CardDescription>
      </CardHeader>
      <CardContent>
        {!config.writable && (
          <div className="mb-4 rounded-md border border-amber-300 bg-amber-50 dark:bg-amber-500/10 text-amber-800 dark:text-amber-300 text-sm p-3">
            claudear.toml is read-only for this process, so deploy QA settings cannot be saved from the console.
            Edit the file where it is mounted and restart claudear instead.
          </div>
        )}
        <form onSubmit={handleSubmit}>
          <fieldset disabled={!config.writable || saving} className="space-y-6 min-w-0 disabled:opacity-70">
            <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
              <div className="flex items-center justify-between gap-3 rounded-md border p-3">
                <div>
                  <p className="text-sm font-medium">Enabled</p>
                  <p className="text-xs text-muted-foreground">Poll the tracks for new release tags.</p>
                </div>
                <Toggle label="Enabled" checked={draft.enabled} onChange={enabled => update({ enabled })} />
              </div>
              <div className="flex items-center justify-between gap-3 rounded-md border p-3">
                <div>
                  <p className="text-sm font-medium">Skip if previous running</p>
                  <p className="text-xs text-muted-foreground">Wait for a track's running QA before starting the next tag.</p>
                </div>
                <Toggle
                  label="Skip if previous running"
                  checked={draft.skip_if_previous_running}
                  onChange={skip => update({ skip_if_previous_running: skip })}
                />
              </div>
              <div className="space-y-1">
                <label htmlFor="deploy-qa-poll-interval" className="text-sm font-medium">
                  Poll interval (ms)
                </label>
                <input
                  id="deploy-qa-poll-interval"
                  type="number"
                  min={0}
                  step={1}
                  value={draft.poll_interval_ms}
                  onChange={event => update({ poll_interval_ms: event.target.value })}
                  className={inputClassName}
                />
                <FieldIssue message={issueFor('poll_interval_ms')} />
              </div>
              <div className="space-y-1">
                <label htmlFor="deploy-qa-timeout" className="text-sm font-medium">
                  Timeout (seconds)
                </label>
                <input
                  id="deploy-qa-timeout"
                  type="number"
                  min={0}
                  step={1}
                  value={draft.timeout_secs}
                  onChange={event => update({ timeout_secs: event.target.value })}
                  className={inputClassName}
                />
                <FieldIssue message={issueFor('timeout_secs')} />
              </div>
              <div className="space-y-1">
                <label htmlFor="deploy-qa-discord-channel" className="text-sm font-medium">
                  Discord channel ID
                </label>
                <input
                  id="deploy-qa-discord-channel"
                  value={draft.discord_channel_id}
                  onChange={event => update({ discord_channel_id: event.target.value })}
                  placeholder="Optional"
                  className={inputClassName}
                />
              </div>
              <div className="space-y-1">
                <label htmlFor="deploy-qa-discord-map" className="text-sm font-medium">
                  GitHub to Discord map path
                </label>
                <input
                  id="deploy-qa-discord-map"
                  value={draft.github_discord_map_path}
                  onChange={event => update({ github_discord_map_path: event.target.value })}
                  placeholder="Optional"
                  className={inputClassName}
                />
              </div>
              <div className="space-y-1 md:col-span-2">
                <label htmlFor="deploy-qa-instructions" className="text-sm font-medium">
                  Instructions path
                </label>
                <input
                  id="deploy-qa-instructions"
                  value={draft.instructions_path}
                  onChange={event => update({ instructions_path: event.target.value })}
                  placeholder="Optional; the bundled playbook is used when empty"
                  className={inputClassName}
                />
              </div>
            </div>

            <div className="space-y-3">
              <div className="flex items-center justify-between">
                <p className="text-sm font-medium">Tracks</p>
                <button
                  type="button"
                  onClick={addTrack}
                  className="inline-flex items-center gap-1 px-3 py-1.5 text-sm border rounded-md hover:bg-muted"
                >
                  <Plus className="h-4 w-4" />
                  Add track
                </button>
              </div>
              {draft.tracks.length === 0 && (
                <p className="text-sm text-muted-foreground">No tracks. Add one to start watching a repository.</p>
              )}
              {draft.tracks.map((track, index) => (
                <TrackEditor
                  key={track.id}
                  index={index}
                  track={track}
                  kinds={kinds}
                  issueFor={issueFor}
                  onChange={change => updateTrack(track.id, change)}
                  onRemove={() => removeTrack(track.id)}
                />
              ))}
            </div>

            {failure && (
              <div role="alert" className="rounded-md bg-destructive/10 text-destructive text-sm p-3 space-y-2">
                <p>{failure.message}</p>
                {failure.code === REVISION_MISMATCH_CODE && (
                  <div className="flex flex-wrap items-center gap-2">
                    <span>claudear.toml changed since this page loaded. Reload to edit the latest settings.</span>
                    <button
                      type="button"
                      onClick={onReload}
                      className="inline-flex items-center gap-1 px-2 py-1 text-xs border rounded-md hover:bg-muted"
                    >
                      <RotateCcw className="h-3 w-3" />
                      Reload
                    </button>
                  </div>
                )}
              </div>
            )}

            <div className="flex items-center justify-end gap-3">
              {issues.length > 0 && (
                <span className="text-xs text-destructive">Fix the errors above to save.</span>
              )}
              <button
                type="submit"
                disabled={issues.length > 0}
                className="inline-flex items-center gap-2 px-3 py-2 bg-primary text-primary-foreground rounded-md text-sm font-medium hover:bg-primary/90 disabled:opacity-50 disabled:cursor-not-allowed"
              >
                {saving ? <Loader2 className="h-4 w-4 animate-spin" /> : <Save className="h-4 w-4" />}
                Save
              </button>
            </div>
          </fieldset>
        </form>
      </CardContent>
    </Card>
  )
}

export default function DeployQaPage() {
  const { user } = useAuth()
  const isAdmin = user?.role === ADMIN_ROLE
  const { mutate } = useSWRConfig()
  const [track, setTrack] = useState('')
  const [saved, setSaved] = useState<DeployQaSaveResponse | null>(null)

  const status = useSWR<DeployQaStatus>(STATUS_KEY, fetchDeployQaStatus, {
    refreshInterval: REFRESH_INTERVAL_MS,
  })
  const config = useSWR<DeployQaConfigResponse>(isAdmin ? CONFIG_KEY : null, fetchDeployQaConfig, {
    revalidateOnFocus: false,
    revalidateOnReconnect: false,
  })

  const restartRequired =
    Boolean(status.data?.restart_required) || Boolean(config.data?.restart_required) || Boolean(saved?.restart_required)

  const handleSaved = (response: DeployQaSaveResponse) => {
    setSaved(response)
    void mutate(STATUS_KEY)
    void mutate(CONFIG_KEY)
    void mutate(key => Array.isArray(key) && key[0] === TIPS_KEY)
  }

  return (
    <div className="space-y-6">
      <PageHeader title="Deploy QA" description="Live QA of new release tags" />

      {restartRequired && (
        <div
          role="status"
          className="flex items-start gap-2 rounded-md border border-amber-300 bg-amber-50 dark:bg-amber-500/10 text-amber-800 dark:text-amber-300 text-sm p-3"
        >
          <AlertTriangle className="h-4 w-4 mt-0.5 shrink-0" />
          <span>
            claudear.toml has deploy QA changes that are not running yet. Restart claudear to apply them.
          </span>
        </div>
      )}

      {saved && (
        <div className="flex items-start gap-2 rounded-md bg-green-500/10 text-green-700 dark:text-green-400 text-sm p-3">
          <CheckCircle2 className="h-4 w-4 mt-0.5 shrink-0" />
          <span>{saved.message}</span>
        </div>
      )}

      {status.error ? (
        <div className="text-destructive text-sm">Failed to load deploy QA status.</div>
      ) : status.isLoading || !status.data ? (
        <BlockSkeleton className="h-40 w-full" />
      ) : (
        <>
          <StatusCard status={status.data} />
          <TracksCard tracks={status.data.tracks} />
        </>
      )}

      <RecentRunsCard tracks={status.data?.tracks ?? []} track={track} onTrackChange={setTrack} />

      {isAdmin &&
        (config.error ? (
          <div className="text-destructive text-sm">Failed to load deploy QA settings.</div>
        ) : config.data ? (
          <SettingsCard
            key={config.data.revision}
            config={config.data}
            onSaved={handleSaved}
            onReload={() => void mutate(CONFIG_KEY)}
          />
        ) : (
          <BlockSkeleton className="h-64 w-full" />
        ))}
    </div>
  )
}
