import { useId, useState } from 'react'
import useSWR, { useSWRConfig } from 'swr'
import { Check, RotateCcw } from 'lucide-react'
import {
  ApiError,
  SENSITIVE_IDENTITY_FIELDS,
  fetchUserIdentities,
  resetUserIdentities,
  updateUserIdentities,
  type Identities,
  type IdentitiesUpdate,
  type IdentityFieldName,
  type IdentityOrigin,
  type IdentityViolation,
  type UserIdentities,
} from '../../lib/api'
import { cn } from '../../lib/utils'
import { Badge } from '../ui/badge'

export const IDENTITIES_KEY = 'identities'

export function userIdentitiesKey(userId: number) {
  return ['user-identities', userId] as const
}

const LIST_FIELDS = [
  'linear_names',
  'github_usernames',
  'sentry_usernames',
  'jira_usernames',
  'gitlab_usernames',
] as const satisfies ReadonlyArray<keyof Identities>

type ListField = (typeof LIST_FIELDS)[number]
type Draft = Record<keyof Identities, string>

interface FieldSpec {
  label: string
  placeholder: string
}

const FIELDS: Record<keyof Identities, FieldSpec> = {
  linear_names: { label: 'Linear', placeholder: 'Jake Barnby' },
  github_usernames: { label: 'GitHub', placeholder: 'octocat' },
  sentry_usernames: { label: 'Sentry', placeholder: 'jake' },
  jira_usernames: { label: 'Jira', placeholder: 'jake.barnby' },
  gitlab_usernames: { label: 'GitLab', placeholder: 'jake' },
  discord_id: { label: 'Discord ID', placeholder: '452316113016193024' },
  slack_id: { label: 'Slack ID', placeholder: 'U0123456789' },
  email: { label: 'Email', placeholder: 'jake@example.com' },
  push_user_key: { label: 'Pushover key', placeholder: '30-character user key' },
  sms_number: { label: 'SMS', placeholder: '+14155550123' },
  whatsapp_number: { label: 'WhatsApp', placeholder: '+14155550123' },
  telegram_chat_id: { label: 'Telegram chat ID', placeholder: '123456789' },
}

const GROUPS: ReadonlyArray<{ title: string; description: string; fields: ReadonlyArray<keyof Identities> }> = [
  {
    title: 'Source handles',
    description: 'Comma-separated. Used to match issue assignees and PR authors to this person.',
    fields: LIST_FIELDS,
  },
  {
    title: 'Chat',
    description: 'Who to mention in Discord and Slack.',
    fields: ['discord_id', 'slack_id'],
  },
  {
    title: 'Notifications',
    description: 'Where direct notifications for this person are delivered.',
    fields: ['email', 'push_user_key', 'sms_number', 'whatsapp_number', 'telegram_chat_id'],
  },
]

const IDENTITY_FIELDS = GROUPS.flatMap((group) => group.fields)

const ORIGIN_BADGE_CLASSES: Record<IdentityOrigin, string> = {
  console: 'bg-primary/10 text-primary',
  config: 'bg-muted text-muted-foreground',
  unset: 'border border-input bg-background text-muted-foreground',
}

const INPUT_CLASS =
  'w-full px-3 py-2 border rounded-md bg-background text-sm focus:outline-none focus:ring-2 focus:ring-primary disabled:opacity-50 disabled:cursor-not-allowed read-only:bg-muted read-only:text-muted-foreground'

function isListField(field: keyof Identities): field is ListField {
  return (LIST_FIELDS as ReadonlyArray<keyof Identities>).includes(field)
}

function isHidden(identities: UserIdentities, field: keyof Identities) {
  return identities.redacted && SENSITIVE_IDENTITY_FIELDS.includes(field)
}

function parseList(value: string) {
  return value.split(',').map((item) => item.trim()).filter((item) => item !== '')
}

function sameList(left: string[], right: string[]) {
  return left.length === right.length && left.every((item, index) => item === right[index])
}

function toDraft(identities: Identities): Draft {
  const draft = {} as Draft
  for (const field of IDENTITY_FIELDS) {
    const value = identities[field]
    draft[field] = Array.isArray(value) ? value.join(', ') : value ?? ''
  }
  return draft
}

function buildUpdate(
  current: UserIdentities,
  draft: Draft,
  slug: string,
  canEditSlug: boolean,
): IdentitiesUpdate {
  const update: IdentitiesUpdate = {}
  if (canEditSlug && slug.trim() !== (current.slug ?? '')) {
    update.slug = slug.trim()
  }
  for (const field of IDENTITY_FIELDS) {
    if (isHidden(current, field)) continue
    if (isListField(field)) {
      const next = parseList(draft[field])
      if (!sameList(next, current.identities[field])) update[field] = next
    } else {
      const next = draft[field].trim()
      if (next !== (current.identities[field] ?? '')) update[field] = next
    }
  }
  return update
}

function groupViolations(violations: IdentityViolation[]) {
  const grouped = new Map<IdentityFieldName, string[]>()
  for (const violation of violations) {
    grouped.set(violation.field, [...(grouped.get(violation.field) ?? []), violation.message])
  }
  return grouped
}

export function OriginBadge({ identities }: { identities: UserIdentities }) {
  const label = {
    console: 'Console',
    config: identities.slug ? `From config (${identities.slug})` : 'From config',
    unset: 'Not set',
  }[identities.origin]
  return <Badge className={cn('font-medium', ORIGIN_BADGE_CLASSES[identities.origin])}>{label}</Badge>
}

export function IdentityForm({ userId, canEditSlug }: { userId: number; canEditSlug: boolean }) {
  const key = userIdentitiesKey(userId)
  const { data, error, isLoading, mutate } = useSWR<UserIdentities>(key, () => fetchUserIdentities(userId))
  const { mutate: mutateGlobal } = useSWRConfig()
  const [notice, setNotice] = useState('')

  if (isLoading) {
    return <div className="text-xs text-muted-foreground">Loading identities…</div>
  }
  if (error || !data) {
    return <div className="bg-destructive/10 text-destructive text-sm p-3 rounded-md">Failed to load identities</div>
  }

  const handleSaved = async (saved: UserIdentities, message: string) => {
    await mutate(saved, { revalidate: false })
    void mutateGlobal(IDENTITIES_KEY)
    setNotice(message)
    setTimeout(() => setNotice(''), 2000)
  }

  return (
    <IdentityEditor
      key={`${data.origin}:${data.updated_at ?? ''}`}
      identities={data}
      canEditSlug={canEditSlug}
      notice={notice}
      onSaved={handleSaved}
    />
  )
}

function IdentityEditor({
  identities,
  canEditSlug,
  notice,
  onSaved,
}: {
  identities: UserIdentities
  canEditSlug: boolean
  notice: string
  onSaved: (saved: UserIdentities, message: string) => Promise<void>
}) {
  const formId = useId()
  const [draft, setDraft] = useState<Draft>(() => toDraft(identities.identities))
  const [slug, setSlug] = useState(identities.slug ?? '')
  const [violations, setViolations] = useState<Map<IdentityFieldName, string[]>>(new Map())
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)

  const update = buildUpdate(identities, draft, slug, canEditSlug)
  const dirty = Object.keys(update).length > 0

  const fail = (caught: unknown, fallback: string) => {
    if (caught instanceof ApiError) {
      setViolations(groupViolations(caught.violations))
      setError(caught.message)
    } else {
      setViolations(new Map())
      setError(fallback)
    }
  }

  const succeed = async (saved: UserIdentities, message: string) => {
    setViolations(new Map())
    setError('')
    setDraft(toDraft(saved.identities))
    setSlug(saved.slug ?? '')
    await onSaved(saved, message)
  }

  const handleSubmit = async (event: React.FormEvent) => {
    event.preventDefault()
    if (!dirty) return
    setBusy(true)
    try {
      await succeed(await updateUserIdentities(identities.user_id, update), 'Identities saved')
    } catch (caught) {
      fail(caught, 'Failed to save identities')
    } finally {
      setBusy(false)
    }
  }

  const handleRevert = async () => {
    if (!confirm('Revert to config? Identities saved in the console for this person will be removed.')) return
    setBusy(true)
    try {
      await succeed(await resetUserIdentities(identities.user_id), 'Reverted to config')
    } catch (caught) {
      fail(caught, 'Failed to revert identities')
    } finally {
      setBusy(false)
    }
  }

  const renderErrors = (field: IdentityFieldName) => {
    const messages = violations.get(field)
    if (!messages) return null
    return (
      <div id={`${formId}-${field}-error`} className="text-xs text-destructive space-y-0.5">
        {messages.map((message) => <p key={message}>{message}</p>)}
      </div>
    )
  }

  const inputProps = (field: IdentityFieldName) => ({
    id: `${formId}-${field}`,
    'aria-invalid': violations.has(field) || undefined,
    'aria-describedby': violations.has(field) ? `${formId}-${field}-error` : undefined,
    className: cn(INPUT_CLASS, violations.has(field) && 'border-destructive'),
  })

  return (
    <form onSubmit={handleSubmit} className="space-y-5">
      <div className="flex items-center gap-2 flex-wrap">
        <OriginBadge identities={identities} />
        {identities.origin === 'config' && (
          <span className="text-xs text-muted-foreground">Saving copies these values into the console.</span>
        )}
        {identities.redacted && (
          <span className="text-xs text-muted-foreground">Some fields are hidden.</span>
        )}
      </div>

      {error && <div className="bg-destructive/10 text-destructive text-sm p-3 rounded-md">{error}</div>}

      <div className="space-y-1 max-w-md">
        <label htmlFor={`${formId}-slug`} className="text-sm font-medium">Person key</label>
        <input
          {...inputProps('slug')}
          value={slug}
          onChange={(event) => setSlug(event.target.value)}
          readOnly={!canEditSlug}
          placeholder={canEditSlug ? 'jake' : 'Assigned automatically'}
        />
        <p className="text-xs text-muted-foreground">
          {canEditSlug
            ? 'Links this person to a [users.<key>] config entry.'
            : 'Only admins can change the person key.'}
        </p>
        {renderErrors('slug')}
      </div>

      {GROUPS.map((group) => (
        <fieldset key={group.title} className="space-y-3">
          <div>
            <legend className="text-sm font-medium">{group.title}</legend>
            <p className="text-xs text-muted-foreground">{group.description}</p>
          </div>
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
            {group.fields.map((field) => {
              const hidden = isHidden(identities, field)
              return (
                <div key={field} className="space-y-1">
                  <label htmlFor={`${formId}-${field}`} className="text-sm font-medium">{FIELDS[field].label}</label>
                  <input
                    {...inputProps(field)}
                    value={hidden ? '' : draft[field]}
                    onChange={(event) => setDraft({ ...draft, [field]: event.target.value })}
                    disabled={hidden}
                    placeholder={hidden ? 'Hidden' : FIELDS[field].placeholder}
                  />
                  {renderErrors(field)}
                </div>
              )
            })}
          </div>
        </fieldset>
      ))}

      <div className="flex items-center justify-end gap-2">
        {notice && (
          <span className="text-green-500 text-sm flex items-center gap-1 mr-auto">
            <Check className="h-3 w-3" /> {notice}
          </span>
        )}
        {identities.origin === 'console' && (
          <button
            type="button"
            onClick={handleRevert}
            disabled={busy}
            className="flex items-center gap-1.5 px-3 py-2 border rounded-md text-sm hover:bg-muted disabled:opacity-50"
          >
            <RotateCcw className="h-4 w-4" /> Revert to config
          </button>
        )}
        <button
          type="submit"
          disabled={busy || !dirty}
          className="px-3 py-2 bg-primary text-primary-foreground rounded-md text-sm font-medium hover:bg-primary/90 disabled:opacity-50"
        >
          {busy ? 'Saving...' : 'Save identities'}
        </button>
      </div>
    </form>
  )
}
