import { useState } from 'react'
import useSWR from 'swr'
import {
  fetchIssues,
  fetchAttempts,
  fetchIssueTimeline,
  type IssuesResponse,
  type IssueSummary,
  type AttemptsResponse,
  type AttemptSummary,
  type IssueTimeline,
} from '../lib/api'
import { PageHeader } from '../components/layout/page-header'
import { DataTable, type Column } from '../components/shared/data-table'
import { StatusBadge } from '../components/shared/status-badge'
import { TimeAgo } from '../components/shared/time-ago'
import { Modal } from '../components/shared/modal'
import { TableRowsSkeleton } from '../components/shared/page-skeletons'
import { Card, CardContent } from '../components/ui/card'
import { formatDate, parseUTCDate } from '../lib/formatters'

export default function IssuesPage() {
  const [selectedIssue, setSelectedIssue] = useState<IssueSummary | null>(null)

  const { data, error, isLoading } = useSWR<IssuesResponse>(
    'issues-list',
    () => fetchIssues({ per_page: 100 }),
    { refreshInterval: 30000 },
  )

  // Fetch attempts for the selected issue when the modal opens
  const { data: attemptsData } = useSWR<AttemptsResponse>(
    selectedIssue
      ? `issues-attempts-${selectedIssue.source}-${selectedIssue.issue_id}`
      : null,
    () =>
      fetchAttempts({
        source: selectedIssue!.source,
        per_page: 100,
      }),
    { refreshInterval: 0 },
  )

  // Fetch the activity timeline for the selected issue when the modal opens
  const { data: timelineData } = useSWR<IssueTimeline>(
    selectedIssue
      ? `issue-timeline-${selectedIssue.source}-${selectedIssue.issue_id}`
      : null,
    () => fetchIssueTimeline(selectedIssue!.source, selectedIssue!.issue_id),
    { refreshInterval: 0 },
  )

  // Filter attempts for the selected issue
  const issueAttempts: AttemptSummary[] = attemptsData
    ? attemptsData.attempts.filter(
        a =>
          a.source === selectedIssue?.source &&
          (a.short_id === selectedIssue?.short_id ||
            a.short_id === selectedIssue?.issue_id),
      )
    : []

  const columns: Column<IssueSummary>[] = [
    {
      key: 'source',
      header: 'Source',
      render: row => (
        <span className="px-2 py-0.5 rounded text-xs font-medium bg-blue-500/10 text-blue-700 dark:text-blue-400 capitalize">
          {row.source}
        </span>
      ),
    },
    {
      key: 'short_id',
      header: 'Issue ID',
      render: row => (
        <span className="font-mono text-sm">{row.short_id || row.issue_id}</span>
      ),
    },
    {
      key: 'title',
      header: 'Title',
      render: row => {
        const title = row.title || row.short_id || row.issue_id
        return (
          <span className="text-sm" title={title}>
            {title.length > 60 ? title.slice(0, 60) + '...' : title}
          </span>
        )
      },
      className: 'max-w-sm',
    },
    {
      key: 'priority',
      header: 'Priority',
      render: row => (
        <span className="text-sm capitalize">{row.priority || 'none'}</span>
      ),
    },
    {
      key: 'status',
      header: 'Status',
      render: row => <StatusBadge status={row.status || 'open'} />,
    },
    {
      key: 'url',
      header: 'Link',
      render: row =>
        row.url ? (
          <a
            href={row.url}
            target="_blank"
            rel="noopener noreferrer"
            className="text-primary hover:underline text-xs"
            onClick={e => e.stopPropagation()}
          >
            View
          </a>
        ) : null,
    },
    {
      key: 'created_at',
      header: 'Created',
      render: row => <TimeAgo date={row.created_at} />,
      sortable: true,
    },
  ]

  return (
    <div className="space-y-6">
      <PageHeader
        title="Issues"
        description={`Tracked issues from connected sources${data ? ` (${data.total} total)` : ''}`}
      />

      {error && (
        <div className="text-destructive text-sm">Failed to load issues.</div>
      )}

      {isLoading && (
        <TableRowsSkeleton rows={6} />
      )}

      {data && (
        <Card>
          <CardContent className="p-4">
            <DataTable
              columns={columns}
              data={data.issues}
              keyFn={row => `${row.source}:${row.issue_id}`}
              emptyMessage="No issues found"
              onRowClick={row => setSelectedIssue(row)}
            />
          </CardContent>
        </Card>
      )}

      <Modal
        open={!!selectedIssue}
        onClose={() => setSelectedIssue(null)}
        title={
          selectedIssue
            ? `${selectedIssue.source}: ${selectedIssue.short_id || selectedIssue.issue_id}`
            : undefined
        }
      >
        {selectedIssue && (
          <div className="space-y-4">
            <div className="grid gap-3 sm:grid-cols-2">
              <div>
                <p className="text-sm text-muted-foreground">Source</p>
                <p className="text-sm capitalize">{selectedIssue.source}</p>
              </div>
              <div>
                <p className="text-sm text-muted-foreground">Issue ID</p>
                <p className="text-sm font-mono">
                  {selectedIssue.short_id || selectedIssue.issue_id}
                </p>
              </div>
              <div className="sm:col-span-2">
                <p className="text-sm text-muted-foreground">Title</p>
                <p className="text-sm">
                  {selectedIssue.title || selectedIssue.short_id || selectedIssue.issue_id}
                </p>
              </div>
              {selectedIssue.description && (
                <div className="sm:col-span-2">
                  <p className="text-sm text-muted-foreground">Description</p>
                  <p className="text-sm whitespace-pre-wrap line-clamp-6">
                    {selectedIssue.description}
                  </p>
                </div>
              )}
              <div>
                <p className="text-sm text-muted-foreground">Priority</p>
                <p className="text-sm capitalize">{selectedIssue.priority || 'none'}</p>
              </div>
              <div>
                <p className="text-sm text-muted-foreground">Status</p>
                <StatusBadge status={selectedIssue.status || 'open'} />
              </div>
              {selectedIssue.labels && selectedIssue.labels.length > 0 && (
                <div className="sm:col-span-2">
                  <p className="text-sm text-muted-foreground">Labels</p>
                  <div className="flex flex-wrap gap-1 mt-1">
                    {selectedIssue.labels.map(label => (
                      <span
                        key={label}
                        className="px-2 py-0.5 rounded text-xs bg-muted text-muted-foreground"
                      >
                        {label}
                      </span>
                    ))}
                  </div>
                </div>
              )}
              {selectedIssue.url && (
                <div className="sm:col-span-2">
                  <p className="text-sm text-muted-foreground">URL</p>
                  <a
                    href={selectedIssue.url}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="text-sm text-primary hover:underline break-all"
                  >
                    {selectedIssue.url}
                  </a>
                </div>
              )}
            </div>

            {issueAttempts.length > 0 && (
              <div>
                <p className="text-sm font-medium mb-2">
                  Fix Attempts ({issueAttempts.length})
                </p>
                <div className="space-y-2">
                  {issueAttempts
                    .sort(
                      (a, b) =>
                        parseUTCDate(b.attempted_at).getTime() -
                        parseUTCDate(a.attempted_at).getTime(),
                    )
                    .map(a => (
                      <div
                        key={a.id}
                        className="flex items-center gap-3 border rounded-md p-2 text-sm"
                      >
                        <StatusBadge status={a.status} />
                        <span className="text-muted-foreground">
                          {formatDate(a.attempted_at)}
                        </span>
                        {a.pr_url && (
                          <a
                            href={a.pr_url}
                            target="_blank"
                            rel="noopener noreferrer"
                            className="text-primary hover:underline text-xs"
                          >
                            PR
                          </a>
                        )}
                        {a.retry_count > 0 && (
                          <span className="text-xs text-muted-foreground">
                            retry #{a.retry_count}
                          </span>
                        )}
                      </div>
                    ))}
                </div>
              </div>
            )}

            {timelineData && timelineData.events.length > 0 && (
              <div>
                <p className="text-sm font-medium mb-2">
                  Timeline ({timelineData.events.length})
                </p>
                <div className="relative space-y-3 border-l border-border pl-4">
                  {timelineData.events.map((event, i) => (
                    <div key={`${event.time}-${i}`} className="relative">
                      <span className="absolute -left-[21px] top-1.5 h-2 w-2 rounded-full bg-primary" />
                      <div className="flex flex-wrap items-center gap-2">
                        <span className="px-2 py-0.5 rounded text-xs font-medium bg-muted text-muted-foreground">
                          {event.event}
                        </span>
                        <span className="text-xs text-muted-foreground">
                          {formatDate(event.time)}
                        </span>
                      </div>
                      {typeof event.message === 'string' && event.message && (
                        <p className="text-sm mt-0.5">{event.message}</p>
                      )}
                    </div>
                  ))}
                </div>
              </div>
            )}
          </div>
        )}
      </Modal>
    </div>
  )
}
