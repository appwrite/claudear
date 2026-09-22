import { useState } from 'react'
import useSWR from 'swr'
import {
  fetchAnalyticsSummary,
  fetchMetrics,
  fetchSupportReplies,
  rateReply,
  type AnalyticsSummary,
  type ProcessingMetric,
  type SupportReply,
} from '../lib/api'
import { useAuth } from '../lib/auth'
import { parseUTCDate, formatMins } from '../lib/formatters'
import { PageHeader } from '../components/layout/page-header'
import { BlockSkeleton, StatsGridSkeleton } from '../components/shared/page-skeletons'
import { StatsCard } from '../components/shared/stats-card'
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '../components/ui/card'
import {
  BarChart3,
  Clock,
  CheckCircle2,
  AlertTriangle,
  DollarSign,
  TrendingUp,
  GitCommit,
  Star,
  MessageSquare,
  Timer,
} from 'lucide-react'
import {
  LineChart,
  Line,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
} from 'recharts'

/** Inline 1–5 star control. Read-only unless `editable`. */
function StarRating({
  value,
  editable,
  onRate,
}: {
  value: number | null | undefined
  editable: boolean
  onRate: (rating: number) => void
}) {
  const [hover, setHover] = useState(0)
  const current = value ?? 0
  return (
    <div className="flex items-center gap-0.5" onMouseLeave={() => setHover(0)}>
      {[1, 2, 3, 4, 5].map(n => {
        const filled = (hover || current) >= n
        return (
          <button
            key={n}
            type="button"
            disabled={!editable}
            onMouseEnter={() => editable && setHover(n)}
            onClick={() => editable && onRate(n)}
            className={editable ? 'cursor-pointer' : 'cursor-default'}
            aria-label={`Rate ${n} star${n > 1 ? 's' : ''}`}
          >
            <Star
              className={`h-4 w-4 ${filled ? 'fill-yellow-400 text-yellow-400' : 'text-muted-foreground'}`}
            />
          </button>
        )
      })}
    </div>
  )
}

export default function AnalyticsPage() {
  const { user } = useAuth()
  const isAdmin = user?.role === 'admin'

  const { data: summary, isLoading: summaryLoading } = useSWR<AnalyticsSummary>(
    'analytics-summary',
    fetchAnalyticsSummary,
    { refreshInterval: 30000 },
  )

  const { data: metrics, isLoading: metricsLoading } = useSWR<ProcessingMetric[]>(
    'processing-time-metrics',
    () => fetchMetrics({ name: 'processing_time', period: 'week', limit: 100 }),
    { refreshInterval: 30000 },
  )

  const {
    data: replies,
    isLoading: repliesLoading,
    mutate: mutateReplies,
  } = useSWR<SupportReply[]>('support-replies', fetchSupportReplies, {
    refreshInterval: 30000,
  })

  const chartData = (metrics || []).map(m => ({
    time: parseUTCDate(m.timestamp).toLocaleString(undefined, {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    }),
    value: m.metric_value,
  }))

  const support = summary?.support_rating

  async function handleRate(reply: SupportReply, rating: number, note?: string) {
    try {
      await rateReply(reply.action_run_id, rating, note ?? reply.note ?? undefined)
      await mutateReplies()
    } catch {
      // postJson already reports to Sentry; surface nothing further here.
    }
  }

  return (
    <div className="space-y-6">
      <PageHeader title="Analytics" description="Trends, metrics, and performance insights" />

      {summaryLoading && (
        <StatsGridSkeleton count={8} className="md:grid-cols-3 xl:grid-cols-4" />
      )}

      {summary && (
        <>
          <div className="grid gap-4 md:grid-cols-3 xl:grid-cols-4">
            <StatsCard
              title="Success Rate"
              value={`${summary.success_rate.toFixed(1)}%`}
              icon={<CheckCircle2 className="h-4 w-4 text-green-500" />}
              description={`${summary.total_successful} of ${summary.total_processed} successful`}
            />
            <StatsCard
              title="Total Processed"
              value={summary.total_processed}
              icon={<BarChart3 className="h-4 w-4 text-primary" />}
              description={`${summary.total_merged} merged`}
            />
            <StatsCard
              title="Avg Processing Time"
              value={
                summary.avg_processing_time_secs != null
                  ? `${summary.avg_processing_time_secs.toFixed(1)}s`
                  : '--'
              }
              icon={<Clock className="h-4 w-4 text-yellow-500" />}
              description="Mean time per attempt"
            />
            <StatsCard
              title="Most Common Error"
              value={summary.most_common_error || 'None'}
              icon={<AlertTriangle className="h-4 w-4 text-red-500" />}
              description="Top recurring error"
            />
            <StatsCard
              title="Avg Time to PR"
              value={formatMins(summary.avg_time_to_pr_mins ?? null)}
              icon={<Clock className="h-4 w-4 text-indigo-500" />}
              description="Issue → PR creation"
            />
            <StatsCard
              title="Est. Cost / Fix"
              value={
                summary.cost_estimate
                  ? `$${summary.cost_estimate.avg_cost_per_fix.toFixed(3)}`
                  : '--'
              }
              icon={<DollarSign className="h-4 w-4 text-emerald-500" />}
              description={
                summary.cost_estimate
                  ? `$${summary.cost_estimate.total_cost.toFixed(2)} total · ${summary.cost_estimate.cost_source} (${summary.cost_estimate.period})`
                  : 'No cost data'
              }
            />
            <StatsCard
              title="Avg Support Rating"
              value={support?.avg_rating != null ? `${support.avg_rating.toFixed(1)} / 5` : '--'}
              icon={<Star className="h-4 w-4 text-yellow-400" />}
              description={
                support
                  ? `${support.rated_count} of ${support.total_replies} replies rated`
                  : 'No replies yet'
              }
            />
            <StatsCard
              title="Avg Response Time"
              value={formatMins(support?.avg_response_time_mins ?? null)}
              icon={<Timer className="h-4 w-4 text-sky-500" />}
              description="Received → reply sent"
            />
          </div>

          {Object.keys(summary.success_rate_by_source).length > 0 && (
            <Card>
              <CardHeader>
                <CardTitle>Success Rate by Source</CardTitle>
                <CardDescription>Percentage of successful attempts per source</CardDescription>
              </CardHeader>
              <CardContent>
                <div className="space-y-3">
                  {Object.entries(summary.success_rate_by_source)
                    .sort(([, a], [, b]) => b - a)
                    .map(([source, rate]) => (
                      <div key={source} className="flex items-center gap-3">
                        <span className="w-24 text-sm font-medium capitalize">{source}</span>
                        <div className="flex-1 h-6 bg-muted rounded overflow-hidden">
                          <div
                            className="h-full bg-green-500 rounded transition-all"
                            style={{ width: `${Math.min(rate, 100)}%` }}
                          />
                        </div>
                        <span className="w-16 text-sm text-right font-medium">
                          {rate.toFixed(1)}%
                        </span>
                      </div>
                    ))}
                </div>
              </CardContent>
            </Card>
          )}

          {(summary.commit_trend ?? []).length > 0 && (
            <Card>
              <CardHeader>
                <CardTitle className="flex items-center gap-2">
                  <GitCommit className="h-5 w-5" />
                  Commits per Day
                </CardTitle>
                <CardDescription>
                  Agent executions that produced a commit (last 30 days)
                </CardDescription>
              </CardHeader>
              <CardContent>
                <div className="h-64">
                  <ResponsiveContainer width="100%" height="100%">
                    <LineChart data={summary.commit_trend}>
                      <CartesianGrid strokeDasharray="3 3" className="stroke-muted" />
                      <XAxis dataKey="period_start" tick={{ fontSize: 12 }} className="text-muted-foreground" />
                      <YAxis tick={{ fontSize: 12 }} className="text-muted-foreground" allowDecimals={false} label={{ value: 'commits', angle: -90, position: 'insideLeft', style: { fontSize: 12 } }} />
                      <Tooltip />
                      <Line type="monotone" dataKey="commits" stroke="#10b981" strokeWidth={2} dot={{ r: 3 }} name="Commits" />
                    </LineChart>
                  </ResponsiveContainer>
                </div>
              </CardContent>
            </Card>
          )}

          {(summary.mttr_trend ?? []).length > 0 && (
            <Card>
              <CardHeader>
                <CardTitle className="flex items-center gap-2">
                  <TrendingUp className="h-5 w-5" />
                  MTTR Trend
                </CardTitle>
                <CardDescription>Weekly mean time to resolve (minutes)</CardDescription>
              </CardHeader>
              <CardContent>
                <div className="h-64">
                  <ResponsiveContainer width="100%" height="100%">
                    <LineChart data={summary.mttr_trend}>
                      <CartesianGrid strokeDasharray="3 3" className="stroke-muted" />
                      <XAxis dataKey="period_start" tick={{ fontSize: 12 }} className="text-muted-foreground" />
                      <YAxis tick={{ fontSize: 12 }} className="text-muted-foreground" label={{ value: 'minutes', angle: -90, position: 'insideLeft', style: { fontSize: 12 } }} />
                      <Tooltip />
                      <Line type="monotone" dataKey="mttr_minutes" stroke="#8b5cf6" strokeWidth={2} dot={{ r: 3 }} name="MTTR (min)" />
                    </LineChart>
                  </ResponsiveContainer>
                </div>
              </CardContent>
            </Card>
          )}

          {(summary.repo_leaderboard ?? []).length > 0 && (
            <Card>
              <CardHeader>
                <CardTitle>Repo Leaderboard</CardTitle>
                <CardDescription>Per-repository fix performance</CardDescription>
              </CardHeader>
              <CardContent>
                <div className="overflow-x-auto">
                  <table className="w-full text-sm">
                    <thead>
                      <tr className="border-b">
                        <th className="text-left py-2 font-medium">#</th>
                        <th className="text-left py-2 font-medium">Repository</th>
                        <th className="text-right py-2 font-medium">Total</th>
                        <th className="text-right py-2 font-medium">Success Rate</th>
                        <th className="text-right py-2 font-medium">Merge Rate</th>
                        <th className="text-right py-2 font-medium">Avg Time to Merge</th>
                      </tr>
                    </thead>
                    <tbody>
                      {summary.repo_leaderboard!.map((entry, i) => (
                        <tr key={entry.repo} className="border-b last:border-0">
                          <td className="py-2">{i + 1}</td>
                          <td className="py-2 font-medium">{entry.repo}</td>
                          <td className="text-right py-2">{entry.total}</td>
                          <td className="text-right py-2 text-green-600">{(entry.success_rate * 100).toFixed(1)}%</td>
                          <td className="text-right py-2 text-purple-600">{(entry.merge_rate * 100).toFixed(1)}%</td>
                          <td className="text-right py-2">{formatMins(entry.avg_time_to_merge_mins ?? null)}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </CardContent>
            </Card>
          )}
        </>
      )}

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <MessageSquare className="h-5 w-5" />
            Support Responses
          </CardTitle>
          <CardDescription>
            Replies sent to QA channels and HelpScout.
            {isAdmin ? ' Click the stars to rate reply quality.' : ' Ratings are admin-only.'}
          </CardDescription>
        </CardHeader>
        <CardContent>
          {repliesLoading && <BlockSkeleton className="h-48 w-full" />}
          {!repliesLoading && (replies ?? []).length === 0 && (
            <p className="text-sm text-muted-foreground py-8 text-center">
              No support replies recorded yet
            </p>
          )}
          {!repliesLoading && (replies ?? []).length > 0 && (
            <div className="overflow-x-auto">
              <table className="w-full text-sm [&_th]:px-3 [&_td]:px-3 [&_th:first-child]:pl-0 [&_td:first-child]:pl-0 [&_th:last-child]:pr-0 [&_td:last-child]:pr-0">
                <thead>
                  <tr className="border-b">
                    <th className="text-left py-2 font-medium">Source</th>
                    <th className="text-left py-2 font-medium">ID</th>
                    <th className="text-left py-2 font-medium">Kind</th>
                    <th className="text-left py-2 font-medium">Reply</th>
                    <th className="text-right py-2 font-medium whitespace-nowrap">Response Time</th>
                    <th className="text-left py-2 font-medium">Sent</th>
                    <th className="text-left py-2 font-medium">Rating</th>
                  </tr>
                </thead>
                <tbody>
                  {replies!.map(reply => (
                    <tr key={reply.action_run_id} className="border-b last:border-0 align-top">
                      <td className="py-2 capitalize">{reply.source}</td>
                      <td className="py-2 font-medium">{reply.short_id}</td>
                      <td className="py-2">{reply.kind.replace(/_/g, ' ')}</td>
                      <td className="py-2 max-w-md truncate text-muted-foreground" title={reply.detail ?? ''}>
                        {reply.detail || '--'}
                      </td>
                      <td className="text-right py-2">{formatMins(reply.response_time_mins ?? null)}</td>
                      <td className="py-2">{parseUTCDate(reply.created_at).toLocaleDateString()}</td>
                      <td className="py-2">
                        <StarRating
                          value={reply.rating}
                          editable={isAdmin}
                          onRate={n => handleRate(reply, n)}
                        />
                        {reply.note && (
                          <p className="text-xs text-muted-foreground mt-1 max-w-xs truncate" title={reply.note}>
                            “{reply.note}”
                          </p>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <TrendingUp className="h-5 w-5" />
            Processing Time
          </CardTitle>
          <CardDescription>Time series of processing durations</CardDescription>
        </CardHeader>
        <CardContent>
          {metricsLoading && <BlockSkeleton className="h-64 w-full" />}
          {!metricsLoading && chartData.length === 0 && (
            <p className="text-sm text-muted-foreground py-8 text-center">
              No metrics data available yet
            </p>
          )}
          {!metricsLoading && chartData.length > 0 && (
            <div className="h-64">
              <ResponsiveContainer width="100%" height="100%">
                <LineChart data={chartData}>
                  <CartesianGrid strokeDasharray="3 3" className="stroke-muted" />
                  <XAxis
                    dataKey="time"
                    tick={{ fontSize: 12 }}
                    className="text-muted-foreground"
                  />
                  <YAxis
                    tick={{ fontSize: 12 }}
                    className="text-muted-foreground"
                    label={{
                      value: 'seconds',
                      angle: -90,
                      position: 'insideLeft',
                      style: { fontSize: 12 },
                    }}
                  />
                  <Tooltip />
                  <Line
                    type="monotone"
                    dataKey="value"
                    stroke="hsl(var(--primary))"
                    strokeWidth={2}
                    dot={false}
                  />
                </LineChart>
              </ResponsiveContainer>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  )
}
