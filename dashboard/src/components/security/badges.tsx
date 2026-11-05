import type {
  AwardStatus,
  DeliveryStatus,
  SecurityOutcome,
  SecurityReportState,
  SecuritySeverity,
  VulnerabilityState,
} from '../../lib/api'

type Tone = 'neutral' | 'info' | 'progress' | 'warning' | 'danger' | 'success' | 'merged'

const toneClasses: Record<Tone, string> = {
  neutral: 'bg-zinc-500/10 text-zinc-700 dark:text-zinc-400',
  info: 'bg-blue-500/10 text-blue-700 dark:text-blue-400',
  progress: 'bg-cyan-500/10 text-cyan-700 dark:text-cyan-400',
  warning: 'bg-amber-500/10 text-amber-700 dark:text-amber-400',
  danger: 'bg-red-500/10 text-red-700 dark:text-red-400',
  success: 'bg-green-500/10 text-green-700 dark:text-green-400',
  merged: 'bg-purple-500/10 text-purple-700 dark:text-purple-400',
}

const reportStateTones: Record<SecurityReportState, Tone> = {
  received: 'info',
  assessing: 'progress',
  needs_human: 'warning',
  registering: 'progress',
  spam: 'neutral',
  replying: 'progress',
  closed: 'neutral',
  tracking: 'merged',
  resolved: 'success',
}

const outcomeTones: Record<SecurityOutcome, Tone> = {
  spam: 'neutral',
  valid: 'danger',
  not_reproducible: 'neutral',
  already_fixed: 'info',
  not_vulnerability: 'neutral',
  out_of_scope: 'neutral',
  duplicate: 'merged',
}

const vulnerabilityStateTones: Record<VulnerabilityState, Tone> = {
  awaiting_fix: 'warning',
  fixing: 'progress',
  merged: 'merged',
  live: 'success',
  needs_human: 'danger',
}

const severityTones: Record<SecuritySeverity, Tone> = {
  critical: 'danger',
  high: 'danger',
  medium: 'warning',
  low: 'info',
  informational: 'neutral',
}

const deliveryTones: Record<DeliveryStatus, Tone> = {
  sending: 'progress',
  sent: 'success',
  failed: 'danger',
  unconfirmed: 'warning',
}

const awardTones: Record<AwardStatus, Tone> = {
  prepared: 'warning',
  owner_notified: 'warning',
  awarded: 'success',
  cancelled: 'neutral',
}

export function formatLabel(value: string): string {
  const words = value.replace(/_/g, ' ')
  return words.charAt(0).toUpperCase() + words.slice(1)
}

function ToneBadge({ tone, label }: { tone: Tone; label: string }) {
  return (
    <span className={`inline-block whitespace-nowrap px-2 py-1 rounded text-xs font-medium font-mono ${toneClasses[tone]}`}>
      {label}
    </span>
  )
}

export function ReportStateBadge({ state }: { state: SecurityReportState }) {
  return <ToneBadge tone={reportStateTones[state] ?? 'neutral'} label={state.replace(/_/g, ' ')} />
}

export function OutcomeBadge({ outcome }: { outcome: SecurityOutcome }) {
  return <ToneBadge tone={outcomeTones[outcome] ?? 'neutral'} label={outcome.replace(/_/g, ' ')} />
}

export function VulnerabilityStateBadge({ state }: { state: VulnerabilityState }) {
  return <ToneBadge tone={vulnerabilityStateTones[state] ?? 'neutral'} label={state.replace(/_/g, ' ')} />
}

export function SeverityBadge({ severity }: { severity: SecuritySeverity }) {
  return <ToneBadge tone={severityTones[severity] ?? 'neutral'} label={severity} />
}

export function DeliveryBadge({ status }: { status: DeliveryStatus }) {
  return <ToneBadge tone={deliveryTones[status] ?? 'neutral'} label={status} />
}

export function AwardBadge({ status }: { status: AwardStatus }) {
  return <ToneBadge tone={awardTones[status] ?? 'neutral'} label={status.replace(/_/g, ' ')} />
}

export function FirstReporterBadge() {
  return (
    <span className="inline-block whitespace-nowrap px-1.5 py-0.5 rounded text-[10px] font-semibold uppercase tracking-wider bg-primary/10 text-primary">
      First reporter
    </span>
  )
}
