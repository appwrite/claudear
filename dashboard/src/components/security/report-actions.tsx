import { useId, useState, type FormEvent, type ReactNode } from 'react'
import { CheckCircle2, GitPullRequest, Rocket, RotateCcw, ShieldCheck, XCircle } from 'lucide-react'
import {
  ApiError,
  SECURITY_OUTCOMES,
  SECURITY_SEVERITIES,
  approveSecurityFix,
  linkSecurityPullRequest,
  markSecurityAwardGranted,
  markSecurityLive,
  overrideSecurityReport,
  type SecurityAward,
  type SecurityOverrideOutcome,
  type SecurityReply,
  type SecurityReport,
  type SecuritySeverity,
  type SecurityVulnerability,
} from '../../lib/api'
import { formatDate } from '../../lib/formatters'
import { Tabs } from '../ui/tabs'
import { AwardBadge, formatLabel } from './badges'

const TITLE_LIMIT = 200
const SUMMARY_LIMIT = 5000
const PUBLIC_REASON_LIMIT = 1000
const DEPLOYED_VERSION_LIMIT = 100
const NOTE_LIMIT = 1000
const REPOSITORY_PATTERN = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/

const overrideOutcomes = SECURITY_OUTCOMES.filter(
  (outcome): outcome is SecurityOverrideOutcome => outcome !== 'valid',
)
const outcomesNeedingReason: SecurityOverrideOutcome[] = ['not_vulnerability', 'out_of_scope']

const inputClass =
  'w-full px-3 py-2 border rounded-md bg-background text-sm focus:outline-none focus:ring-2 focus:ring-primary'
const primaryButtonClass =
  'inline-flex items-center gap-1.5 px-3 py-2 bg-primary text-primary-foreground rounded-md text-sm font-medium hover:bg-primary/90 disabled:opacity-50 disabled:cursor-not-allowed'
const secondaryButtonClass =
  'inline-flex items-center gap-1.5 px-3 py-2 border rounded-md text-sm hover:bg-muted disabled:opacity-50 disabled:cursor-not-allowed'

export function actionErrorMessage(error: unknown): string {
  if (!(error instanceof ApiError)) return 'Request failed. Check your connection and try again.'
  switch (error.status) {
    case 400:
      return 'The server rejected the input. Check the fields and try again.'
    case 403:
      return 'Administrator privileges are required for this action.'
    case 404:
      return 'This record no longer exists.'
    case 409:
      return 'This record changed state since it was loaded. It has been refreshed; review it and try again.'
    default:
      return `Request failed (${error.status}).`
  }
}

function useSecurityAction(onChanged: () => Promise<unknown>) {
  const [pending, setPending] = useState(false)
  const [error, setError] = useState('')

  const run = async (action: () => Promise<unknown>): Promise<boolean> => {
    setPending(true)
    setError('')
    try {
      await action()
      await onChanged()
      return true
    } catch (caught) {
      setError(actionErrorMessage(caught))
      if (caught instanceof ApiError && caught.status === 409) await onChanged()
      return false
    } finally {
      setPending(false)
    }
  }

  return { pending, error, setError, run }
}

function ActionError({ message }: { message: string }) {
  if (!message) return null
  return (
    <div role="alert" className="bg-destructive/10 text-destructive text-sm p-3 rounded-md">
      {message}
    </div>
  )
}

function Field({ label, htmlFor, hint, children }: { label: string; htmlFor: string; hint?: string; children: ReactNode }) {
  return (
    <div className="space-y-1">
      <label htmlFor={htmlFor} className="text-sm font-medium">
        {label}
      </label>
      {children}
      {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
    </div>
  )
}

export function ActionSection({ title, description, children }: { title: string; description?: string; children: ReactNode }) {
  return (
    <section className="border rounded-lg p-4 space-y-3">
      <div>
        <h3 className="text-sm font-semibold">{title}</h3>
        {description && <p className="text-xs text-muted-foreground">{description}</p>}
      </div>
      {children}
    </section>
  )
}

const needsHumanTabs = [
  { value: 'confirm_valid', label: 'Confirm valid' },
  { value: 'outcome', label: 'Close with outcome' },
  { value: 'reassess', label: 'Reassess' },
]

export function NeedsHumanPanel({ report, onChanged }: { report: SecurityReport; onChanged: () => Promise<unknown> }) {
  return (
    <ActionSection
      title="Needs a decision"
      description="The automated assessment could not settle this report. Choose how to proceed."
    >
      <Tabs tabs={needsHumanTabs} defaultTab="confirm_valid">
        {activeTab => {
          if (activeTab === 'outcome') return <OutcomeForm report={report} onChanged={onChanged} />
          if (activeTab === 'reassess') return <ReassessAction report={report} onChanged={onChanged} />
          return <ConfirmValidForm report={report} onChanged={onChanged} />
        }}
      </Tabs>
    </ActionSection>
  )
}

function ConfirmValidForm({ report, onChanged }: { report: SecurityReport; onChanged: () => Promise<unknown> }) {
  const id = useId()
  const [title, setTitle] = useState('')
  const [summary, setSummary] = useState(report.assessment_summary ?? '')
  const [repository, setRepository] = useState(report.affected_repository ?? '')
  const [severity, setSeverity] = useState<SecuritySeverity | ''>('')
  const { pending, error, setError, run } = useSecurityAction(onChanged)

  const handleSubmit = async (event: FormEvent) => {
    event.preventDefault()
    const trimmedTitle = title.trim()
    const trimmedSummary = summary.trim()
    const trimmedRepository = repository.trim()
    if (!trimmedTitle || !trimmedSummary) {
      setError('A title and a summary are required.')
      return
    }
    if (trimmedRepository && !REPOSITORY_PATTERN.test(trimmedRepository)) {
      setError('The repository must look like owner/name.')
      return
    }
    if (!window.confirm('Confirm this report as a valid vulnerability? The reporter will receive the confirmation reply.')) return
    await run(() =>
      overrideSecurityReport(report.id, {
        action: 'confirm_valid',
        title: trimmedTitle,
        summary: trimmedSummary,
        ...(trimmedRepository ? { repository: trimmedRepository } : {}),
        ...(severity ? { severity } : {}),
      }),
    )
  }

  return (
    <form onSubmit={handleSubmit} className="space-y-3">
      <ActionError message={error} />
      <Field label="Title" htmlFor={`${id}-title`} hint="Neutral wording; it is used for the fix pull request.">
        <input
          id={`${id}-title`}
          value={title}
          onChange={event => setTitle(event.target.value)}
          maxLength={TITLE_LIMIT}
          required
          className={inputClass}
        />
      </Field>
      <Field label="Summary" htmlFor={`${id}-summary`} hint="What is wrong and where. Never paste reporter payloads verbatim.">
        <textarea
          id={`${id}-summary`}
          value={summary}
          onChange={event => setSummary(event.target.value)}
          maxLength={SUMMARY_LIMIT}
          rows={4}
          required
          className={inputClass}
        />
      </Field>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Repository" htmlFor={`${id}-repository`}>
          <input
            id={`${id}-repository`}
            value={repository}
            onChange={event => setRepository(event.target.value)}
            placeholder="owner/name"
            className={`${inputClass} font-mono`}
          />
        </Field>
        <Field label="Severity" htmlFor={`${id}-severity`}>
          <select
            id={`${id}-severity`}
            value={severity}
            onChange={event => setSeverity(event.target.value as SecuritySeverity | '')}
            className={inputClass}
          >
            <option value="">Not set</option>
            {SECURITY_SEVERITIES.map(value => (
              <option key={value} value={value}>
                {formatLabel(value)}
              </option>
            ))}
          </select>
        </Field>
      </div>
      <div className="flex justify-end">
        <button type="submit" disabled={pending} className={primaryButtonClass}>
          <CheckCircle2 className="h-4 w-4" />
          {pending ? 'Confirming...' : 'Confirm vulnerability'}
        </button>
      </div>
    </form>
  )
}

function OutcomeForm({ report, onChanged }: { report: SecurityReport; onChanged: () => Promise<unknown> }) {
  const id = useId()
  const [outcome, setOutcome] = useState<SecurityOverrideOutcome>('not_reproducible')
  const [publicReason, setPublicReason] = useState('')
  const [fixedOn, setFixedOn] = useState('')
  const { pending, error, setError, run } = useSecurityAction(onChanged)

  const needsReason = outcomesNeedingReason.includes(outcome)
  const needsFixedOn = outcome === 'already_fixed'
  const isSpam = outcome === 'spam'

  const handleSubmit = async (event: FormEvent) => {
    event.preventDefault()
    const trimmedReason = publicReason.trim()
    if (needsReason && !trimmedReason) {
      setError('A public reason is required for this outcome.')
      return
    }
    if (needsFixedOn && !fixedOn) {
      setError('The date the issue was fixed is required.')
      return
    }
    const message = isSpam
      ? 'Mark this report as spam?'
      : `Close this report as "${formatLabel(outcome)}"? The reporter will receive the matching reply.`
    if (!window.confirm(message)) return
    await run(() =>
      overrideSecurityReport(report.id, {
        action: 'outcome',
        outcome,
        ...(needsReason ? { public_reason: trimmedReason } : {}),
        ...(needsFixedOn ? { fixed_on: fixedOn } : {}),
      }),
    )
  }

  return (
    <form onSubmit={handleSubmit} className="space-y-3">
      <ActionError message={error} />
      <Field label="Outcome" htmlFor={`${id}-outcome`}>
        <select
          id={`${id}-outcome`}
          value={outcome}
          onChange={event => setOutcome(event.target.value as SecurityOverrideOutcome)}
          className={inputClass}
        >
          {overrideOutcomes.map(value => (
            <option key={value} value={value}>
              {formatLabel(value)}
            </option>
          ))}
        </select>
      </Field>
      {needsReason && (
        <Field label="Public reason" htmlFor={`${id}-reason`} hint="Quoted in the reply to the reporter.">
          <textarea
            id={`${id}-reason`}
            value={publicReason}
            onChange={event => setPublicReason(event.target.value)}
            maxLength={PUBLIC_REASON_LIMIT}
            rows={3}
            required
            className={inputClass}
          />
        </Field>
      )}
      {needsFixedOn && (
        <Field label="Fixed on" htmlFor={`${id}-fixed-on`}>
          <input
            id={`${id}-fixed-on`}
            type="date"
            value={fixedOn}
            onChange={event => setFixedOn(event.target.value)}
            required
            className={inputClass}
          />
        </Field>
      )}
      <div className="flex justify-end">
        <button type="submit" disabled={pending} className={primaryButtonClass}>
          <XCircle className="h-4 w-4" />
          {pending ? 'Saving...' : isSpam ? 'Mark as spam' : 'Send outcome'}
        </button>
      </div>
    </form>
  )
}

function ReassessAction({ report, onChanged }: { report: SecurityReport; onChanged: () => Promise<unknown> }) {
  const { pending, error, run } = useSecurityAction(onChanged)

  return (
    <div className="space-y-3">
      <ActionError message={error} />
      <p className="text-sm text-muted-foreground">
        Return the report to the queue for a fresh automated assessment. The attempt counter is reset
        ({report.assessment_attempts} so far).
      </p>
      <div className="flex justify-end">
        <button
          type="button"
          disabled={pending}
          onClick={() => void run(() => overrideSecurityReport(report.id, { action: 'reassess' }))}
          className={secondaryButtonClass}
        >
          <RotateCcw className="h-4 w-4" />
          {pending ? 'Reassessing...' : 'Start reassessment'}
        </button>
      </div>
    </div>
  )
}

export function notDeliveredPrompt(reply: SecurityReply): string {
  return `Record the ${formatLabel(reply.kind).toLowerCase()} reply as not delivered? Claudear will send it to the reporter again.`
}

export function awardedPrompt(award: SecurityAward): string {
  const recipient = award.recipient_email ?? 'the first reporter'
  return `Record the ${award.campaign} reward (${award.credits} credits) as granted to ${recipient}? Only confirm after the coupon or credits were actually sent. This cannot be undone.`
}

export function DeliveryConfirmation({
  report,
  replies,
  onChanged,
}: {
  report: SecurityReport
  replies: SecurityReply[]
  onChanged: () => Promise<unknown>
}) {
  const { pending, error, run } = useSecurityAction(onChanged)
  const unconfirmed = replies.filter(reply => reply.status === 'unconfirmed')
  if (unconfirmed.length === 0) return null

  return (
    <ActionSection
      title="Confirm delivery"
      description="HelpScout did not confirm these replies. Check the conversation, then record whether each one reached the reporter. Unconfirmed replies are never retried automatically."
    >
      <ActionError message={error} />
      <ul className="space-y-2">
        {unconfirmed.map(reply => (
          <li key={reply.id} className="flex flex-wrap items-center justify-between gap-2 text-sm">
            <span>
              <span className="font-medium">{formatLabel(reply.kind)}</span>
              <span className="text-muted-foreground"> reply, claimed {formatDate(reply.claimed_at)}</span>
            </span>
            <span className="flex gap-2">
              <button
                type="button"
                disabled={pending}
                onClick={() =>
                  void run(() =>
                    overrideSecurityReport(report.id, { action: 'confirm_delivery', kind: reply.kind, delivered: true }),
                  )
                }
                className={secondaryButtonClass}
              >
                Delivered
              </button>
              <button
                type="button"
                disabled={pending}
                onClick={() => {
                  if (!window.confirm(notDeliveredPrompt(reply))) return
                  void run(() =>
                    overrideSecurityReport(report.id, { action: 'confirm_delivery', kind: reply.kind, delivered: false }),
                  )
                }}
                className={secondaryButtonClass}
              >
                Not delivered
              </button>
            </span>
          </li>
        ))}
      </ul>
    </ActionSection>
  )
}

export function ApproveFixAction({
  vulnerability,
  onChanged,
}: {
  vulnerability: SecurityVulnerability
  onChanged: () => Promise<unknown>
}) {
  const { pending, error, run } = useSecurityAction(onChanged)

  return (
    <ActionSection
      title="Approve fix"
      description="The fix run opens a pull request, which stays public on open-source repositories until Cloud deploys it."
    >
      <div className="space-y-3">
        <ActionError message={error} />
        <div className="flex justify-end">
          <button
            type="button"
            disabled={pending}
            onClick={() => void run(() => approveSecurityFix(vulnerability.id))}
            className={primaryButtonClass}
          >
            <ShieldCheck className="h-4 w-4" />
            {pending ? 'Approving...' : 'Approve fix'}
          </button>
        </div>
      </div>
    </ActionSection>
  )
}

export function LinkPullRequestForm({
  vulnerability,
  onChanged,
}: {
  vulnerability: SecurityVulnerability
  onChanged: () => Promise<unknown>
}) {
  const id = useId()
  const [pullRequestUrl, setPullRequestUrl] = useState('')
  const { pending, error, setError, run } = useSecurityAction(onChanged)

  const handleSubmit = async (event: FormEvent) => {
    event.preventDefault()
    const trimmed = pullRequestUrl.trim()
    if (!trimmed) {
      setError('A pull request URL is required.')
      return
    }
    if (await run(() => linkSecurityPullRequest(vulnerability.id, trimmed))) setPullRequestUrl('')
  }

  return (
    <ActionSection title="Link pull request" description="Track a fix that was opened by hand.">
      <form onSubmit={handleSubmit} className="space-y-3">
        <ActionError message={error} />
        <Field label="Pull request URL" htmlFor={`${id}-pull-request`}>
          <input
            id={`${id}-pull-request`}
            type="url"
            value={pullRequestUrl}
            onChange={event => setPullRequestUrl(event.target.value)}
            placeholder="https://github.com/owner/name/pull/123"
            required
            className={`${inputClass} font-mono`}
          />
        </Field>
        <div className="flex justify-end">
          <button type="submit" disabled={pending} className={primaryButtonClass}>
            <GitPullRequest className="h-4 w-4" />
            {pending ? 'Linking...' : 'Link PR'}
          </button>
        </div>
      </form>
    </ActionSection>
  )
}

export function MarkLiveForm({
  vulnerability,
  onChanged,
}: {
  vulnerability: SecurityVulnerability
  onChanged: () => Promise<unknown>
}) {
  const id = useId()
  const [deployedVersion, setDeployedVersion] = useState('')
  const { pending, error, setError, run } = useSecurityAction(onChanged)

  const handleSubmit = async (event: FormEvent) => {
    event.preventDefault()
    const trimmed = deployedVersion.trim()
    if (!trimmed) {
      setError('The deployed version is required.')
      return
    }
    if (!window.confirm(`Mark the fix live in ${trimmed}? Reporters will be told the fix is live.`)) return
    if (await run(() => markSecurityLive(vulnerability.id, trimmed))) setDeployedVersion('')
  }

  return (
    <ActionSection
      title="Mark live"
      description="Use when the fix is deployed on Cloud but could not be detected automatically."
    >
      <form onSubmit={handleSubmit} className="space-y-3">
        <ActionError message={error} />
        <Field label="Deployed version" htmlFor={`${id}-deployed-version`}>
          <input
            id={`${id}-deployed-version`}
            value={deployedVersion}
            onChange={event => setDeployedVersion(event.target.value)}
            maxLength={DEPLOYED_VERSION_LIMIT}
            placeholder="1.2.3"
            required
            className={`${inputClass} font-mono`}
          />
        </Field>
        <div className="flex justify-end">
          <button type="submit" disabled={pending} className={primaryButtonClass}>
            <Rocket className="h-4 w-4" />
            {pending ? 'Saving...' : 'Mark live'}
          </button>
        </div>
      </form>
    </ActionSection>
  )
}

export function AwardPanel({ award, onChanged }: { award: SecurityAward; onChanged: () => Promise<unknown> }) {
  const id = useId()
  const [note, setNote] = useState('')
  const { pending, error, run } = useSecurityAction(onChanged)
  const awardable = award.status === 'prepared' || award.status === 'owner_notified'

  const handleSubmit = async (event: FormEvent) => {
    event.preventDefault()
    const trimmed = note.trim()
    if (!window.confirm(awardedPrompt(award))) return
    if (await run(() => markSecurityAwardGranted(award.vulnerability_id, trimmed || undefined))) setNote('')
  }

  return (
    <ActionSection title="Reward" description="Prepared for the first valid reporter only.">
      <dl className="grid gap-3 sm:grid-cols-2 text-sm">
        <div>
          <dt className="text-muted-foreground">Status</dt>
          <dd><AwardBadge status={award.status} /></dd>
        </div>
        <div>
          <dt className="text-muted-foreground">Recipient</dt>
          <dd>{award.recipient_email ?? '--'}</dd>
        </div>
        <div>
          <dt className="text-muted-foreground">Campaign</dt>
          <dd className="font-mono">{award.campaign}</dd>
        </div>
        <div>
          <dt className="text-muted-foreground">Credits</dt>
          <dd>{award.credits}</dd>
        </div>
        <div>
          <dt className="text-muted-foreground">Prepared</dt>
          <dd>{formatDate(award.prepared_at)}</dd>
        </div>
        {award.actor && (
          <div>
            <dt className="text-muted-foreground">Awarded by</dt>
            <dd>{award.actor}</dd>
          </div>
        )}
        {award.note && (
          <div className="sm:col-span-2">
            <dt className="text-muted-foreground">Note</dt>
            <dd className="whitespace-pre-wrap">{award.note}</dd>
          </div>
        )}
      </dl>
      {awardable && (
        <form onSubmit={handleSubmit} className="space-y-3 border-t pt-3">
          <ActionError message={error} />
          <p className="text-xs text-muted-foreground">
            Send the {award.campaign} coupon or grant the credits in Cloud first, then record it here.
          </p>
          <Field label="Note (optional)" htmlFor={`${id}-note`}>
            <input
              id={`${id}-note`}
              value={note}
              onChange={event => setNote(event.target.value)}
              maxLength={NOTE_LIMIT}
              placeholder="How and when it was granted"
              className={inputClass}
            />
          </Field>
          <div className="flex justify-end">
            <button type="submit" disabled={pending} className={primaryButtonClass}>
              <CheckCircle2 className="h-4 w-4" />
              {pending ? 'Saving...' : 'Mark awarded'}
            </button>
          </div>
        </form>
      )}
    </ActionSection>
  )
}
