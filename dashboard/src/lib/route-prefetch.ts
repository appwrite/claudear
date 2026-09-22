import { preload } from 'swr'
import {
  fetchActivity,
  fetchAnalyticsSummary,
  fetchAttempts,
  fetchConfig,
  fetchDependencies,
  fetchDiscordChannels,
  fetchDiscordChannelStats,
  fetchErrors,
  fetchExperiments,
  fetchFeedback,
  fetchInferenceHistory,
  fetchInferenceStats,
  fetchIssues,
  fetchMetrics,
  fetchOverview,
  fetchPrAnalytics,
  fetchPrs,
  fetchRegressions,
  fetchRepos,
  fetchRepoStats,
  fetchTelemetryLatency,
  fetchTelemetryOverview,
  fetchTelemetryPipeline,
  fetchTelemetryTimeseries,
  fetchUsers,
  getRetries,
} from './api'

const prefetchedRoutes = new Set<string>()
const telemetryPeriods = [
  { period: 'hour', bucketMinutes: 5 },
  { period: 'day', bucketMinutes: 15 },
  { period: 'week', bucketMinutes: 60 },
  { period: 'month', bucketMinutes: 360 },
] as const
const regressionStatuses = ['awaiting_release', 'monitoring', 'resolved', 'regressed'] as const
const prStatuses = ['', 'open', 'merged', 'closed'] as const

function queuePreload<T>(key: string | readonly unknown[], fetcher: () => Promise<T>) {
  void preload(key, fetcher).catch(() => {
    // Route prefetch is best-effort; normal page loading handles retries/errors.
  })
}

const routePrefetchers: Record<string, () => void> = {
  '/': () => {
    queuePreload('overview', fetchOverview)
    queuePreload('retries', getRetries)
  },
  '/issues': () => {
    queuePreload('issues-list', () => fetchIssues({ per_page: 100 }))
  },
  '/attempts': () => {
    queuePreload(['attempts', 1, '', ''], () => fetchAttempts({ page: 1, per_page: 20 }))
  },
  '/prs': () => {
    queuePreload('pr-analytics', fetchPrAnalytics)
    for (const status of prStatuses) {
      queuePreload(['prs', status], () => fetchPrs({ status: status || undefined }))
    }
  },
  '/analytics': () => {
    queuePreload('analytics-summary', fetchAnalyticsSummary)
    queuePreload('processing-time-metrics', () =>
      fetchMetrics({ name: 'processing_time', period: 'week', limit: 100 }),
    )
  },
  '/errors': () => {
    queuePreload('errors', () => fetchErrors(50))
  },
  '/feedback': () => {
    queuePreload(['feedback', ''], () => fetchFeedback())
  },
  '/regressions': () => {
    for (const status of regressionStatuses) {
      queuePreload(['regressions', status], () => fetchRegressions(status))
    }
  },
  '/experiments': () => {
    queuePreload('experiments', fetchExperiments)
  },
  '/repos': () => {
    queuePreload('repos', fetchRepos)
    queuePreload('repo-stats', fetchRepoStats)
    queuePreload('dependencies', fetchDependencies)
  },
  '/channels': () => {
    queuePreload('discord-channels', fetchDiscordChannels)
    queuePreload('discord-channel-stats', fetchDiscordChannelStats)
  },
  '/learning': () => {
    queuePreload('repos', fetchRepos)
  },
  '/inference': () => {
    queuePreload('inference-stats', fetchInferenceStats)
    queuePreload('inference-history', () => fetchInferenceHistory(50))
  },
  '/activity': () => {
    queuePreload(['activity', ''], () => fetchActivity({ limit: 200 }))
  },
  '/telemetry': () => {
    queuePreload('telemetry-overview', fetchTelemetryOverview)
    for (const { period, bucketMinutes } of telemetryPeriods) {
      queuePreload(['telemetry-timeseries', period], () =>
        fetchTelemetryTimeseries({ period, bucket_minutes: bucketMinutes }),
      )
      queuePreload(['telemetry-pipeline', period], () =>
        fetchTelemetryPipeline({ period }),
      )
      queuePreload(['telemetry-latency', period], () =>
        fetchTelemetryLatency({ period }),
      )
    }
  },
  '/config': () => {
    queuePreload('config', fetchConfig)
  },
  '/users': () => {
    queuePreload('users', fetchUsers)
  },
}

export function prefetchRouteData(path: string) {
  const prefetch = routePrefetchers[path]
  if (!prefetch || prefetchedRoutes.has(path)) return

  prefetchedRoutes.add(path)
  prefetch()
}

export function resetPrefetchedRoutes() {
  prefetchedRoutes.clear()
}
