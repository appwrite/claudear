import { useState } from 'react'
import useSWR, { useSWRConfig } from 'swr'
import {
  AlertTriangle,
  Award,
  ExternalLink,
  Inbox,
  MailWarning,
  Rocket,
  ShieldAlert,
  Wrench,
} from 'lucide-react'
import {
  SECURITY_OUTCOMES,
  SECURITY_REPORT_STATES,
  fetchSecurityReport,
  fetchSecurityReports,
  fetchSecuritySummary,
  type SecurityOutcome,
  type SecurityReportDetail,
  type SecurityReportRow,
  type SecurityReportState,
  type SecuritySummary,
} from '../lib/api'
import { useAuth } from '../lib/auth'
import { PageHeader } from '../components/layout/page-header'
import { DataTable, type Column } from '../components/shared/data-table'
import { Modal } from '../components/shared/modal'
import { BlockSkeleton, StatsGridSkeleton, TableRowsSkeleton } from '../components/shared/page-skeletons'
import { StatsCard } from '../components/shared/stats-card'
import { TimeAgo } from '../components/shared/time-ago'
import { Card, CardContent } from '../components/ui/card'
import { Select } from '../components/ui/select'
import {
  AwardBadge,
  FirstReporterBadge,
  OutcomeBadge,
  ReportStateBadge,
  SeverityBadge,
  VulnerabilityStateBadge,
  formatLabel,
} from '../components/security/badges'
import { ReportDetail } from '../components/security/report-detail'

const OPEN_REPORT_STATES: SecurityReportState[] = [
  'received',
  'assessing',
  'needs_human',
  'registering',
  'replying',
  'tracking',
]

const stateOptions = SECURITY_REPORT_STATES.map(value => ({ value, label: formatLabel(value) }))
const outcomeOptions = SECURITY_OUTCOMES.map(value => ({ value, label: formatLabel(value) }))

function count(values: Partial<Record<string, number>>, keys: readonly string[]): number {
  return keys.reduce((total, key) => total + (values[key] ?? 0), 0)
}

function Missing() {
  return <span className="text-muted-foreground">--</span>
}

function SummaryCards({ summary }: { summary: SecuritySummary }) {
  return (
    <div className="grid gap-4 md:grid-cols-3 lg:grid-cols-6">
      <StatsCard
        title="Needs Human"
        value={summary.reports_by_state.needs_human ?? 0}
        icon={<AlertTriangle className="h-4 w-4 text-amber-500" />}
        description="Awaiting an admin decision"
      />
      <StatsCard
        title="Open Reports"
        value={count(summary.reports_by_state, OPEN_REPORT_STATES)}
        icon={<Inbox className="h-4 w-4 text-primary" />}
        description="Not yet closed or resolved"
      />
      <StatsCard
        title="Fixes In Flight"
        value={count(summary.vulnerabilities_by_state, ['awaiting_fix', 'fixing', 'merged'])}
        icon={<Wrench className="h-4 w-4 text-cyan-500" />}
        description="Awaiting fix, fixing or merged"
      />
      <StatsCard
        title="Live Fixes"
        value={summary.vulnerabilities_by_state.live ?? 0}
        icon={<Rocket className="h-4 w-4 text-green-500" />}
        description="Deployed on Cloud"
      />
      <StatsCard
        title="Pending Awards"
        value={summary.pending_awards}
        icon={<Award className="h-4 w-4 text-purple-500" />}
        description="Prepared, not yet granted"
      />
      <StatsCard
        title="Unconfirmed Replies"
        value={summary.unconfirmed_replies}
        icon={<MailWarning className="h-4 w-4 text-red-500" />}
        description="Delivery needs checking"
      />
    </div>
  )
}

const columns: Column<SecurityReportRow>[] = [
  {
    key: 'report',
    header: 'Report',
    render: row => (
      <div className="max-w-xs">
        <span className="font-mono text-sm text-primary">{row.short_id}</span>
        <p className="text-xs text-muted-foreground truncate" title={row.subject}>
          {row.subject}
        </p>
      </div>
    ),
  },
  {
    key: 'state',
    header: 'State',
    render: row => (
      <div className="flex flex-col items-start gap-1">
        <ReportStateBadge state={row.state} />
        {row.unconfirmed_reply && (
          <span className="inline-flex items-center gap-1 text-xs font-medium text-amber-700 dark:text-amber-400">
            <AlertTriangle className="h-3 w-3" />
            Unconfirmed reply
          </span>
        )}
      </div>
    ),
  },
  {
    key: 'outcome',
    header: 'Outcome',
    render: row => (row.outcome ? <OutcomeBadge outcome={row.outcome} /> : <Missing />),
  },
  {
    key: 'reporter',
    header: 'Reporter',
    render: row => (
      <div className="flex flex-col items-start gap-1">
        <span className="text-sm">{row.reporter_email ?? 'unknown'}</span>
        {row.is_first_reporter && <FirstReporterBadge />}
      </div>
    ),
  },
  {
    key: 'vulnerability',
    header: 'Vulnerability',
    render: row =>
      row.vulnerability_id != null ? (
        <div className="flex flex-col items-start gap-1">
          <span className="flex items-center gap-1.5">
            <span className="font-mono text-xs text-muted-foreground">#{row.vulnerability_id}</span>
            {row.vulnerability_state && <VulnerabilityStateBadge state={row.vulnerability_state} />}
          </span>
          {row.severity && <SeverityBadge severity={row.severity} />}
        </div>
      ) : (
        <Missing />
      ),
  },
  {
    key: 'pull_request',
    header: 'PR',
    render: row =>
      row.pull_request_url ? (
        <a
          href={row.pull_request_url}
          target="_blank"
          rel="noopener noreferrer"
          className="text-primary hover:underline inline-flex items-center gap-1"
          onClick={event => event.stopPropagation()}
        >
          View
          <ExternalLink className="h-3 w-3" />
        </a>
      ) : (
        <Missing />
      ),
  },
  {
    key: 'deployed_version',
    header: 'Deployed',
    render: row =>
      row.deployed_version ? <span className="font-mono text-xs">{row.deployed_version}</span> : <Missing />,
  },
  {
    key: 'award',
    header: 'Award',
    render: row => (row.award_status ? <AwardBadge status={row.award_status} /> : <Missing />),
  },
  {
    key: 'reported_at',
    header: 'Reported',
    render: row => <TimeAgo date={row.reported_at} />,
  },
]

function AccessDenied() {
  return (
    <div className="flex items-center gap-2 text-muted-foreground text-sm p-6">
      <ShieldAlert className="h-5 w-5" />
      <span>Access denied. You need administrator privileges to view security reports.</span>
    </div>
  )
}

export default function SecurityPage() {
  const { user } = useAuth()
  const isAdmin = user?.role === 'admin'
  const { mutate } = useSWRConfig()
  const [stateFilter, setStateFilter] = useState<SecurityReportState | ''>('')
  const [outcomeFilter, setOutcomeFilter] = useState<SecurityOutcome | ''>('')
  const [selectedId, setSelectedId] = useState<number | null>(null)

  const { data: summary, error: summaryError, isLoading: summaryLoading } = useSWR<SecuritySummary>(
    isAdmin ? 'security-summary' : null,
    fetchSecuritySummary,
    { refreshInterval: 30000 },
  )

  const { data: reports, error: reportsError, isLoading: reportsLoading } = useSWR<SecurityReportRow[]>(
    isAdmin ? ['security-reports', stateFilter, outcomeFilter] : null,
    () =>
      fetchSecurityReports({
        state: stateFilter || undefined,
        outcome: outcomeFilter || undefined,
      }),
    { refreshInterval: 15000 },
  )

  const {
    data: detail,
    error: detailError,
    isLoading: detailLoading,
    mutate: mutateDetail,
  } = useSWR<SecurityReportDetail>(
    isAdmin && selectedId != null ? ['security-report', selectedId] : null,
    () => fetchSecurityReport(selectedId!),
    { refreshInterval: 15000 },
  )

  if (!isAdmin) return <AccessDenied />

  const refresh = () =>
    Promise.all([
      mutateDetail(),
      mutate('security-summary'),
      mutate(key => Array.isArray(key) && key[0] === 'security-reports'),
    ])

  const visibleDetail = detail && detail.report.id === selectedId ? detail : undefined

  return (
    <div className="space-y-6">
      <PageHeader
        title="Security"
        description="Reports to the security inbox, their vulnerabilities, fixes and rewards"
      />

      {summaryLoading && <StatsGridSkeleton count={6} className="md:grid-cols-3 lg:grid-cols-6" />}
      {summaryError && <div className="text-destructive text-sm">Failed to load the security summary.</div>}
      {summary && <SummaryCards summary={summary} />}

      <div className="flex flex-wrap items-center gap-4">
        <Select
          value={stateFilter}
          onChange={value => setStateFilter(value as SecurityReportState | '')}
          options={stateOptions}
          placeholder="All states"
          className="w-44"
        />
        <Select
          value={outcomeFilter}
          onChange={value => setOutcomeFilter(value as SecurityOutcome | '')}
          options={outcomeOptions}
          placeholder="All outcomes"
          className="w-44"
        />
      </div>

      {reportsError && <div className="text-destructive text-sm">Failed to load security reports.</div>}
      {reportsLoading && <TableRowsSkeleton rows={5} />}
      {reports && (
        <Card>
          <CardContent className="p-4">
            <DataTable
              columns={columns}
              data={reports}
              keyFn={row => row.id}
              emptyMessage="No security reports match these filters"
              onRowClick={row => setSelectedId(row.id)}
            />
          </CardContent>
        </Card>
      )}

      <Modal
        open={selectedId != null}
        onClose={() => setSelectedId(null)}
        title={visibleDetail ? `${visibleDetail.report.short_id} security report` : 'Security report'}
      >
        {detailError && <div className="text-destructive text-sm">Failed to load this report.</div>}
        {detailLoading && !visibleDetail && <BlockSkeleton className="h-64 w-full" />}
        {visibleDetail && (
          <ReportDetail detail={visibleDetail} onChanged={refresh} onSelectReport={setSelectedId} />
        )}
      </Modal>
    </div>
  )
}
