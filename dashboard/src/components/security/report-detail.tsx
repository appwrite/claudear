import type { ReactNode } from 'react'
import { ArrowRight, ExternalLink } from 'lucide-react'
import type {
  SecurityEvent,
  SecurityReply,
  SecurityReportDetail,
  SecurityReportRow,
  SecurityVulnerability,
} from '../../lib/api'
import { formatDate } from '../../lib/formatters'
import { TimeAgo } from '../shared/time-ago'
import {
  DeliveryBadge,
  FirstReporterBadge,
  OutcomeBadge,
  ReportStateBadge,
  SeverityBadge,
  VulnerabilityStateBadge,
  formatLabel,
} from './badges'
import {
  ActionSection,
  ApproveFixAction,
  AwardPanel,
  DeliveryConfirmation,
  LinkPullRequestForm,
  MarkLiveForm,
  NeedsHumanPanel,
} from './report-actions'

function Detail({ label, children, wide = false }: { label: string; children: ReactNode; wide?: boolean }) {
  return (
    <div className={wide ? 'sm:col-span-2' : undefined}>
      <dt className="text-sm text-muted-foreground">{label}</dt>
      <dd className="text-sm">{children}</dd>
    </div>
  )
}

function ExternalAnchor({ href, children }: { href: string; children: ReactNode }) {
  return (
    <a
      href={href}
      target="_blank"
      rel="noopener noreferrer"
      className="text-primary hover:underline inline-flex items-center gap-1 break-all"
      onClick={event => event.stopPropagation()}
    >
      {children}
      <ExternalLink className="h-3 w-3 shrink-0" />
    </a>
  )
}

function ReportOverview({ detail }: { detail: SecurityReportDetail }) {
  const { report, vulnerability } = detail
  const isFirstReporter = vulnerability?.first_report_id === report.id

  return (
    <dl className="grid gap-3 sm:grid-cols-2">
      <Detail label="State">
        <ReportStateBadge state={report.state} />
      </Detail>
      <Detail label="Outcome">{report.outcome ? <OutcomeBadge outcome={report.outcome} /> : '--'}</Detail>
      <Detail label="Subject" wide>
        {report.subject}
      </Detail>
      <Detail label="Reporter">
        <span className="inline-flex flex-wrap items-center gap-2">
          {report.reporter_email ?? '--'}
          {isFirstReporter && <FirstReporterBadge />}
        </span>
      </Detail>
      <Detail label="Reported">{formatDate(report.reported_at)}</Detail>
      <Detail label="Conversation">
        <ExternalAnchor href={report.url}>Open in HelpScout</ExternalAnchor>
      </Detail>
      <Detail label="Assessment attempts">{report.assessment_attempts}</Detail>
      {report.spam_confidence != null && (
        <Detail label="Spam confidence">{`${Math.round(report.spam_confidence * 100)}%`}</Detail>
      )}
      {report.affected_repository && (
        <Detail label="Affected repository">
          <span className="font-mono">{report.affected_repository}</span>
        </Detail>
      )}
      {report.fixed_on && <Detail label="Fixed on">{report.fixed_on}</Detail>}
      {report.fixed_by_commit && (
        <Detail label="Fixed by commit">
          <span className="font-mono break-all">{report.fixed_by_commit}</span>
        </Detail>
      )}
      {report.assessment_summary && (
        <Detail label="Assessment" wide>
          <p className="whitespace-pre-wrap break-words">{report.assessment_summary}</p>
        </Detail>
      )}
      {report.public_reason && (
        <Detail label="Public reason" wide>
          <p className="whitespace-pre-wrap break-words">{report.public_reason}</p>
        </Detail>
      )}
    </dl>
  )
}

function VulnerabilitySection({ vulnerability }: { vulnerability: SecurityVulnerability }) {
  return (
    <ActionSection title={`Vulnerability #${vulnerability.id}`}>
      <dl className="grid gap-3 sm:grid-cols-2">
        <Detail label="Title" wide>
          {vulnerability.title}
        </Detail>
        <Detail label="State">
          <VulnerabilityStateBadge state={vulnerability.state} />
        </Detail>
        <Detail label="Severity">
          {vulnerability.severity ? <SeverityBadge severity={vulnerability.severity} /> : '--'}
        </Detail>
        <Detail label="Repository">
          <span className="font-mono">{vulnerability.repository ?? '--'}</span>
        </Detail>
        <Detail label="First reported">{formatDate(vulnerability.first_reported_at)}</Detail>
        <Detail label="Pull request">
          {vulnerability.pull_request_url ? (
            <ExternalAnchor href={vulnerability.pull_request_url}>{vulnerability.pull_request_url}</ExternalAnchor>
          ) : (
            '--'
          )}
        </Detail>
        <Detail label="Merge commit">
          <span className="font-mono break-all">{vulnerability.merge_commit ?? '--'}</span>
        </Detail>
        {vulnerability.merged_at && <Detail label="Merged">{formatDate(vulnerability.merged_at)}</Detail>}
        <Detail label="Deployed version">
          <span className="font-mono">{vulnerability.deployed_version ?? '--'}</span>
        </Detail>
        {vulnerability.live_at && <Detail label="Live since">{formatDate(vulnerability.live_at)}</Detail>}
        <Detail label="Summary" wide>
          <p className="whitespace-pre-wrap break-words">{vulnerability.summary}</p>
        </Detail>
      </dl>
    </ActionSection>
  )
}

function ReplyLedger({ replies }: { replies: SecurityReply[] }) {
  return (
    <ActionSection title="Replies" description="Each reply kind is sent at most once.">
      {replies.length === 0 ? (
        <p className="text-sm text-muted-foreground">No replies yet.</p>
      ) : (
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b text-left">
              <th className="py-2 font-medium">Kind</th>
              <th className="py-2 font-medium">Status</th>
              <th className="py-2 font-medium">Claimed</th>
              <th className="py-2 font-medium">Completed</th>
            </tr>
          </thead>
          <tbody>
            {replies.map(reply => (
              <tr key={reply.id} className="border-b last:border-0 align-top">
                <td className="py-2">
                  {formatLabel(reply.kind)}
                  {reply.error && <p className="text-xs text-destructive break-words">{reply.error}</p>}
                </td>
                <td className="py-2">
                  <DeliveryBadge status={reply.status} />
                </td>
                <td className="py-2">
                  <TimeAgo date={reply.claimed_at} />
                </td>
                <td className="py-2">
                  {reply.completed_at ? <TimeAgo date={reply.completed_at} /> : <span className="text-muted-foreground">--</span>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </ActionSection>
  )
}

function RelatedReports({
  reports,
  onSelect,
}: {
  reports: SecurityReportRow[]
  onSelect: (id: number) => void
}) {
  if (reports.length === 0) return null
  return (
    <ActionSection title="Reports of the same vulnerability">
      <ul className="divide-y">
        {reports.map(related => (
          <li key={related.id}>
            <button
              type="button"
              onClick={() => onSelect(related.id)}
              className="w-full flex flex-wrap items-center gap-2 py-2 text-left text-sm hover:bg-muted/50 rounded"
            >
              <span className="font-mono text-primary">{related.short_id}</span>
              <span className="text-muted-foreground">{related.reporter_email ?? 'unknown reporter'}</span>
              {related.is_first_reporter && <FirstReporterBadge />}
              <span className="ml-auto flex items-center gap-2">
                <ReportStateBadge state={related.state} />
                <TimeAgo date={related.reported_at} />
              </span>
            </button>
          </li>
        ))}
      </ul>
    </ActionSection>
  )
}

function EventDetail({ detail }: { detail: unknown }) {
  if (detail == null) return null
  if (typeof detail !== 'object') {
    return <p className="text-xs text-muted-foreground break-words">{String(detail)}</p>
  }
  const entries = Object.entries(detail as Record<string, unknown>).filter(([, value]) => value != null && value !== '')
  if (entries.length === 0) return null
  return (
    <dl className="mt-1 space-y-0.5 text-xs">
      {entries.map(([key, value]) => (
        <div key={key} className="flex gap-2">
          <dt className="text-muted-foreground shrink-0">{formatLabel(key)}</dt>
          <dd className="font-mono whitespace-pre-wrap break-all">
            {typeof value === 'object' ? JSON.stringify(value) : String(value)}
          </dd>
        </div>
      ))}
    </dl>
  )
}

function Timeline({ events }: { events: SecurityEvent[] }) {
  return (
    <ActionSection title="Timeline" description="Every recorded transition, oldest first.">
      {events.length === 0 ? (
        <p className="text-sm text-muted-foreground">No events recorded.</p>
      ) : (
        <ol className="space-y-3" aria-label="Security timeline">
          {events.map(event => (
            <li key={event.id} className="border-l-2 border-primary/30 pl-3">
              <div className="flex flex-wrap items-center gap-2 text-sm">
                <span className="font-medium">{formatLabel(event.kind)}</span>
                {event.vulnerability_id != null && event.report_id == null && (
                  <span className="text-xs text-muted-foreground font-mono">vulnerability #{event.vulnerability_id}</span>
                )}
                {event.from_state && event.to_state && (
                  <span className="inline-flex items-center gap-1 text-xs font-mono text-muted-foreground">
                    {event.from_state.replace(/_/g, ' ')}
                    <ArrowRight className="h-3 w-3" />
                    {event.to_state.replace(/_/g, ' ')}
                  </span>
                )}
                <span className="ml-auto text-xs text-muted-foreground">
                  {formatDate(event.created_at)}
                </span>
              </div>
              {event.actor && <p className="text-xs text-muted-foreground">by {event.actor}</p>}
              <EventDetail detail={event.detail} />
            </li>
          ))}
        </ol>
      )}
    </ActionSection>
  )
}

export function ReportDetail({
  detail,
  onChanged,
  onSelectReport,
}: {
  detail: SecurityReportDetail
  onChanged: () => Promise<unknown>
  onSelectReport: (id: number) => void
}) {
  const { report, vulnerability, award } = detail
  const canLinkPullRequest = vulnerability?.state === 'awaiting_fix' || vulnerability?.state === 'needs_human'
  const canMarkLive = vulnerability?.state === 'merged' || vulnerability?.state === 'needs_human'

  return (
    <div className="space-y-4">
      <ReportOverview detail={detail} />
      {report.state === 'needs_human' && <NeedsHumanPanel key={report.id} report={report} onChanged={onChanged} />}
      <DeliveryConfirmation report={report} replies={detail.replies} onChanged={onChanged} />
      {vulnerability && <VulnerabilitySection vulnerability={vulnerability} />}
      {vulnerability && detail.awaiting_fix_approval && (
        <ApproveFixAction key={`approve-${vulnerability.id}`} vulnerability={vulnerability} onChanged={onChanged} />
      )}
      {vulnerability && canLinkPullRequest && (
        <LinkPullRequestForm key={`pull-request-${vulnerability.id}`} vulnerability={vulnerability} onChanged={onChanged} />
      )}
      {vulnerability && canMarkLive && (
        <MarkLiveForm key={`live-${vulnerability.id}`} vulnerability={vulnerability} onChanged={onChanged} />
      )}
      {award && <AwardPanel key={award.id} award={award} onChanged={onChanged} />}
      <ReplyLedger replies={detail.replies} />
      <RelatedReports
        reports={detail.related_reports.filter(related => related.id !== report.id)}
        onSelect={onSelectReport}
      />
      <Timeline events={detail.events} />
    </div>
  )
}
