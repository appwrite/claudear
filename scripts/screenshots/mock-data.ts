// Mock API data for dashboard screenshots
// Matches interfaces from dashboard/src/lib/api.ts

const now = new Date('2026-02-22T14:30:00Z');
const iso = (d: Date) => d.toISOString();
const ago = (hours: number) => iso(new Date(now.getTime() - hours * 3600_000));
const daysAgo = (days: number) => ago(days * 24);

// ── Auth ────────────────────────────────────────────────────────────────

export const authMe = {
  id: 1,
  email: 'admin@acme.dev',
  name: 'Admin',
  role: 'admin',
  avatar_url: null,
};

// ── Health ──────────────────────────────────────────────────────────────

export const health = {
  status: 'ok',
  version: '0.14.2',
  uptime_secs: 432_000,
  database: { status: 'ok' },
};

// ── Stats / Overview ────────────────────────────────────────────────────

export const overview = {
  stats: {
    total: 247,
    pending: 3,
    success: 89,
    failed: 18,
    merged: 127,
    closed: 6,
    cannot_fix: 4,
    by_source: {
      linear: { total: 112, success: 42, failed: 8, merged: 57, closed: 3, cannot_fix: 2 },
      sentry: { total: 68, success: 24, failed: 5, merged: 36, closed: 2, cannot_fix: 1 },
      github: { total: 41, success: 15, failed: 3, merged: 21, closed: 1, cannot_fix: 1 },
      discord: { total: 26, success: 8, failed: 2, merged: 13, closed: 0, cannot_fix: 0 },
    },
  },
  success_rate: 87.4,
  merge_rate: 58.8,
  recent_attempts: [
    { id: 247, source: 'linear', short_id: 'LIN-4892', title: 'Fix pagination offset in search results', status: 'success', pr_url: 'https://github.com/acme/api-gateway/pull/891', attempted_at: ago(0.5), retry_count: 0 },
    { id: 246, source: 'sentry', short_id: 'SENTRY-90142', title: 'Handle null pointer in payment webhook handler', status: 'merged', pr_url: 'https://github.com/acme/billing-service/pull/334', attempted_at: ago(2), retry_count: 0 },
    { id: 245, source: 'github', short_id: 'GH-312', title: 'Update deprecated crypto.createCipher to createCipheriv', status: 'merged', pr_url: 'https://github.com/acme/web-frontend/pull/1247', attempted_at: ago(4), retry_count: 0 },
    { id: 244, source: 'linear', short_id: 'LIN-4887', title: 'Add retry logic to external API calls', status: 'success', pr_url: 'https://github.com/acme/api-gateway/pull/889', attempted_at: ago(5), retry_count: 0 },
    { id: 243, source: 'discord', short_id: 'DISC-78', title: 'Fix mobile nav menu not closing on route change', status: 'merged', pr_url: 'https://github.com/acme/web-frontend/pull/1245', attempted_at: ago(7), retry_count: 0 },
    { id: 242, source: 'sentry', short_id: 'SENTRY-89234', title: 'Race condition in WebSocket reconnection logic', status: 'pending', pr_url: null, attempted_at: ago(0.2), retry_count: 0 },
    { id: 241, source: 'linear', short_id: 'LIN-4521', title: 'Fix auth token refresh race condition', status: 'merged', pr_url: 'https://github.com/acme/api-gateway/pull/847', attempted_at: ago(12), retry_count: 0 },
    { id: 240, source: 'sentry', short_id: 'SENTRY-88901', title: 'Fix memory leak in event stream subscription', status: 'failed', pr_url: null, attempted_at: ago(14), retry_count: 1 },
  ],
  sources: [
    { name: 'linear', total: 112, success: 42, failed: 8, merged: 57, success_rate: 88.4 },
    { name: 'sentry', total: 68, success: 24, failed: 5, merged: 36, success_rate: 88.2 },
    { name: 'github', total: 41, success: 15, failed: 3, merged: 21, success_rate: 87.8 },
    { name: 'discord', total: 26, success: 8, failed: 2, merged: 13, success_rate: 80.8 },
  ],
  time_savings: {
    merged_count: 127,
    hours_saved: 381,
    cost_saved: 57_150,
    period: 'all_time',
  },
  agent_spawns_today: 14,
};

// ── Retries ─────────────────────────────────────────────────────────────

export const retries = {
  retryable: [
    { id: 240, source: 'sentry', short_id: 'SENTRY-88901', title: 'Fix memory leak in event stream subscription', status: 'failed', pr_url: null, attempted_at: ago(14), retry_count: 1 },
    { id: 236, source: 'linear', short_id: 'LIN-4510', title: 'Handle edge case in date range validation', status: 'failed', pr_url: null, attempted_at: daysAgo(2), retry_count: 1 },
  ],
  ready: [
    { id: 240, source: 'sentry', short_id: 'SENTRY-88901', title: 'Fix memory leak in event stream subscription', status: 'failed', pr_url: null, attempted_at: ago(14), retry_count: 1 },
    { id: 236, source: 'linear', short_id: 'LIN-4510', title: 'Handle edge case in date range validation', status: 'failed', pr_url: null, attempted_at: daysAgo(2), retry_count: 1 },
  ],
  max_retries: 2,
};

// ── Analytics ───────────────────────────────────────────────────────────

export const analyticsSummary = {
  success_rate: 87.4,
  total_processed: 247,
  total_successful: 216,
  total_merged: 127,
  avg_processing_time_secs: 186,
  avg_time_to_merge_hours: 4.2,
  most_common_error: 'Test suite assertion failure',
  success_rate_by_source: {
    linear: 88.4,
    sentry: 88.2,
    github: 87.8,
    discord: 80.8,
  },
  avg_time_to_pr_mins: 3.1,
  cost_estimate: {
    total_cost: 742.80,
    avg_cost_per_fix: 3.01,
    fix_count: 247,
    cost_source: 'claude_api',
    period: 'all_time',
  },
  mttr_trend: [
    { period_start: daysAgo(42), mttr_minutes: 22.4, sample_count: 18 },
    { period_start: daysAgo(35), mttr_minutes: 19.8, sample_count: 24 },
    { period_start: daysAgo(28), mttr_minutes: 17.1, sample_count: 31 },
    { period_start: daysAgo(21), mttr_minutes: 14.6, sample_count: 28 },
    { period_start: daysAgo(14), mttr_minutes: 12.3, sample_count: 35 },
    { period_start: daysAgo(7), mttr_minutes: 10.8, sample_count: 42 },
    { period_start: daysAgo(0), mttr_minutes: 9.2, sample_count: 38 },
  ],
  repo_leaderboard: [
    { repo: 'acme/api-gateway', total: 87, success_rate: 91.2, merge_rate: 64.4, avg_time_to_merge_mins: 195 },
    { repo: 'acme/web-frontend', total: 62, success_rate: 88.7, merge_rate: 58.1, avg_time_to_merge_mins: 240 },
    { repo: 'acme/billing-service', total: 45, success_rate: 86.7, merge_rate: 55.6, avg_time_to_merge_mins: 310 },
    { repo: 'acme/mobile-app', total: 31, success_rate: 83.9, merge_rate: 51.6, avg_time_to_merge_mins: 420 },
    { repo: 'acme/shared-utils', total: 22, success_rate: 90.9, merge_rate: 68.2, avg_time_to_merge_mins: 145 },
  ],
};

// ── Metrics (processing time chart) ─────────────────────────────────────

export const metrics: Array<{
  id: number;
  timestamp: string;
  metric_name: string;
  metric_value: number;
  source: string | null;
  tags: Record<string, unknown> | null;
}> = Array.from({ length: 30 }, (_, i) => ({
  id: i + 1,
  timestamp: ago((29 - i) * 8),
  metric_name: 'processing_time_secs',
  metric_value: 120 + Math.sin(i * 0.5) * 60 + (Math.random() * 40 - 20),
  source: ['linear', 'sentry', 'github', 'discord'][i % 4],
  tags: null,
}));

// ── PRs ─────────────────────────────────────────────────────────────────

const prStatuses = ['merged', 'merged', 'merged', 'open', 'merged', 'closed', 'merged', 'open', 'merged', 'merged', 'merged', 'closed', 'merged', 'open', 'merged'];
const prRepos = ['acme/api-gateway', 'acme/web-frontend', 'acme/billing-service', 'acme/mobile-app', 'acme/shared-utils'];
const prTitles = [
  'fix: resolve auth token refresh race condition',
  'fix: handle null pointer in payment webhook',
  'feat: add retry logic to external API calls',
  'fix: update deprecated crypto API usage',
  'fix: mobile nav menu not closing on route change',
  'fix: pagination offset in search results',
  'fix: memory leak in event stream subscription',
  'feat: add rate limiting to public endpoints',
  'fix: race condition in WebSocket reconnect',
  'fix: timezone handling in scheduled reports',
  'fix: correct CORS headers for preflight',
  'feat: add health check endpoint',
  'fix: database connection pool exhaustion',
  'fix: incorrect error status codes in REST API',
  'fix: CSS grid layout breaking on Safari',
];

export const prs = prTitles.map((title, i) => ({
  id: i + 1,
  pr_url: `https://github.com/${prRepos[i % 5]}/pull/${800 + i}`,
  scm_repo: prRepos[i % 5],
  pr_number: 800 + i,
  attempt_id: 230 + i,
  issue_id: `issue-${i}`,
  issue_source: ['linear', 'sentry', 'github', 'discord'][i % 4],
  title,
  description: null,
  author: 'claudear[bot]',
  head_branch: `claudear/fix-${800 + i}`,
  base_branch: 'main',
  status: prStatuses[i],
  created_at: daysAgo(15 - i),
  updated_at: daysAgo(14 - i),
  merged_at: prStatuses[i] === 'merged' ? daysAgo(14 - i) : null,
  closed_at: prStatuses[i] === 'closed' ? daysAgo(14 - i) : null,
  approvals_count: prStatuses[i] === 'merged' ? 1 : 0,
  changes_requested_count: prStatuses[i] === 'closed' ? 1 : 0,
  comments_count: Math.floor(Math.random() * 4),
  last_review_at: prStatuses[i] !== 'open' ? daysAgo(14 - i) : null,
  time_to_first_review_mins: 30 + Math.floor(Math.random() * 120),
  time_to_merge_mins: prStatuses[i] === 'merged' ? 180 + Math.floor(Math.random() * 600) : null,
  review_cycles: prStatuses[i] === 'closed' ? 2 : 1,
  files_changed: 2 + Math.floor(Math.random() * 8),
  lines_added: 10 + Math.floor(Math.random() * 150),
  lines_removed: 5 + Math.floor(Math.random() * 80),
}));

export const prAnalytics = {
  total: 15,
  open: 3,
  merged: 10,
  closed: 2,
  avg_time_to_first_review_mins: 72,
  avg_time_to_merge_mins: 348,
  avg_review_cycles: 1.2,
  merge_rate: 66.7,
  by_repo: {
    'acme/api-gateway': 5,
    'acme/web-frontend': 4,
    'acme/billing-service': 3,
    'acme/mobile-app': 2,
    'acme/shared-utils': 1,
  },
  avg_time_to_pr_mins: 3.1,
  rejection_reasons: [
    { category: 'Test failures', count: 3 },
    { category: 'Code style', count: 2 },
    { category: 'Missing edge cases', count: 1 },
    { category: 'Security concern', count: 1 },
  ],
};

// ── Issues ──────────────────────────────────────────────────────────────

const issueSources = ['linear', 'sentry', 'github', 'discord', 'linear', 'sentry', 'github', 'linear', 'sentry', 'discord', 'linear', 'github', 'sentry', 'linear', 'discord'];
const issueIds = [
  'LIN-4892', 'SENTRY-90142', 'GH-312', 'DISC-78', 'LIN-4887',
  'SENTRY-89234', 'GH-308', 'LIN-4521', 'SENTRY-88901', 'DISC-74',
  'LIN-4510', 'GH-299', 'SENTRY-88456', 'LIN-4498', 'DISC-71',
];
const issueTitles = [
  'Fix pagination offset in search results',
  'Null pointer in payment webhook handler',
  'Update deprecated crypto.createCipher',
  'Mobile nav menu not closing on route change',
  'Add retry logic to external API calls',
  'Race condition in WebSocket reconnection',
  'CORS headers missing for preflight requests',
  'Auth token refresh race condition',
  'Memory leak in event stream subscription',
  'Button click handler firing twice on iOS',
  'Date range validation edge case',
  'CSS grid layout breaking on Safari 16',
  'Connection pool exhaustion under load',
  'Incorrect error codes in REST API',
  'Dark mode toggle not persisting preference',
];

export const issues = {
  issues: issueTitles.map((title, i) => ({
    id: i + 1,
    source: issueSources[i],
    issue_id: issueIds[i],
    short_id: issueIds[i],
    title,
    description: `Detailed description for ${title.toLowerCase()}`,
    url: issueSources[i] === 'github'
      ? `https://github.com/acme/web-frontend/issues/${300 + i}`
      : issueSources[i] === 'linear'
        ? `https://linear.app/acme/issue/${issueIds[i]}`
        : null,
    priority: ['urgent', 'high', 'medium', 'low', 'medium'][i % 5],
    status: ['resolved', 'resolved', 'resolved', 'resolved', 'in_progress', 'in_progress', 'resolved', 'resolved', 'failed', 'resolved', 'failed', 'resolved', 'resolved', 'resolved', 'resolved'][i],
    labels: [['bug', 'claudear'], ['error', 'production'], ['claudear'], ['bug', 'mobile'], ['enhancement', 'claudear']][i % 5],
    has_embedding: true,
    created_at: daysAgo(15 - i),
    updated_at: daysAgo(14 - i),
  })),
  total: 15,
  page: 1,
  per_page: 20,
};

// ── Activity ────────────────────────────────────────────────────────────

export const activity = [
  { id: 50, timestamp: ago(0.3), activity_type: 'issue_detected', source: 'linear', issue_id: 'LIN-4892', short_id: 'LIN-4892', message: 'New issue detected: Fix pagination offset in search results', metadata: { confidence: 0.96, repo: 'acme/api-gateway' } },
  { id: 49, timestamp: ago(0.5), activity_type: 'pr_created', source: 'linear', issue_id: 'LIN-4892', short_id: 'LIN-4892', message: 'PR #891 created for LIN-4892', metadata: { pr_url: 'https://github.com/acme/api-gateway/pull/891' } },
  { id: 48, timestamp: ago(2), activity_type: 'pr_merged', source: 'sentry', issue_id: 'SENTRY-90142', short_id: 'SENTRY-90142', message: 'PR #334 merged for SENTRY-90142', metadata: null },
  { id: 47, timestamp: ago(2.5), activity_type: 'issue_resolved', source: 'sentry', issue_id: 'SENTRY-90142', short_id: 'SENTRY-90142', message: 'Issue auto-resolved after PR merge', metadata: null },
  { id: 46, timestamp: ago(4), activity_type: 'pr_merged', source: 'github', issue_id: 'GH-312', short_id: 'GH-312', message: 'PR #1247 merged for GH-312', metadata: null },
  { id: 45, timestamp: ago(5), activity_type: 'agent_spawn', source: 'linear', issue_id: 'LIN-4887', short_id: 'LIN-4887', message: 'Claude Code agent spawned for LIN-4887', metadata: { model: 'sonnet', repo: 'acme/api-gateway' } },
  { id: 44, timestamp: ago(7), activity_type: 'question_asked', source: 'discord', issue_id: 'DISC-78', short_id: 'DISC-78', message: 'Human input needed: Which CSS framework handles the nav?', metadata: { channel: 'discord' } },
  { id: 43, timestamp: ago(7.5), activity_type: 'question_answered', source: 'discord', issue_id: 'DISC-78', short_id: 'DISC-78', message: 'Answer received: Tailwind CSS with custom components', metadata: { responder: 'jake' } },
  { id: 42, timestamp: ago(12), activity_type: 'regression_clear', source: 'linear', issue_id: 'LIN-4521', short_id: 'LIN-4521', message: 'Regression watch cleared for LIN-4521 (24h passed)', metadata: null },
  { id: 41, timestamp: ago(14), activity_type: 'attempt_failed', source: 'sentry', issue_id: 'SENTRY-88901', short_id: 'SENTRY-88901', message: 'Fix attempt failed: test suite assertion failure', metadata: { error: 'Test suite assertion failure' } },
];

// ── Telemetry ───────────────────────────────────────────────────────────

export const telemetryOverview = {
  generated_at: iso(now),
  uptime_secs: 432_000,
  windows: [
    { window: '1h', processed: 4, successful: 4, failed: 0, merged: 1, success_rate: 100, error_rate: 0, throughput_per_hour: 4 },
    { window: '6h', processed: 18, successful: 16, failed: 1, merged: 8, success_rate: 88.9, error_rate: 5.6, throughput_per_hour: 3 },
    { window: '24h', processed: 42, successful: 37, failed: 3, merged: 22, success_rate: 88.1, error_rate: 7.1, throughput_per_hour: 1.75 },
    { window: '7d', processed: 148, successful: 131, failed: 10, merged: 84, success_rate: 88.5, error_rate: 6.8, throughput_per_hour: 0.88 },
  ],
  queue: {
    pending_attempts: 3,
    retryable_attempts: 2,
    ready_retries: 2,
    open_prs: 3,
    watches_awaiting_release: 1,
    watches_monitoring: 4,
    watches_resolved: 89,
    watches_regressed: 2,
  },
  processing_time: {
    all_time: { samples: 244, avg_secs: 186, p50_secs: 162, p95_secs: 420, p99_secs: 780, max_secs: 1260 },
    last_24h: { samples: 42, avg_secs: 174, p50_secs: 155, p95_secs: 395, p99_secs: 710, max_secs: 890 },
  },
  source_breakdown: [
    { source: 'linear', total: 112, pending: 1, success: 42, failed: 8, merged: 57, closed: 3, cannot_fix: 2, retryable: 1, success_rate: 88.4 },
    { source: 'sentry', total: 68, pending: 1, success: 24, failed: 5, merged: 36, closed: 2, cannot_fix: 1, retryable: 1, success_rate: 88.2 },
    { source: 'github', total: 41, pending: 1, success: 15, failed: 3, merged: 21, closed: 1, cannot_fix: 1, retryable: 0, success_rate: 87.8 },
    { source: 'discord', total: 26, pending: 0, success: 8, failed: 2, merged: 13, closed: 0, cannot_fix: 0, retryable: 0, success_rate: 80.8 },
  ],
  top_errors: [
    { id: 1, pattern_hash: 'a1b2c3', error_type: 'test_failure', error_message: 'Test suite assertion failure', first_seen: daysAgo(30), last_seen: ago(14), occurrence_count: 8, sources: ['linear', 'sentry'], example_issue_ids: ['LIN-4510', 'SENTRY-88901'], resolution_hints: 'Check test fixtures and mock data' },
    { id: 2, pattern_hash: 'd4e5f6', error_type: 'timeout', error_message: 'Claude process timed out after 6h', first_seen: daysAgo(21), last_seen: daysAgo(5), occurrence_count: 4, sources: ['linear'], example_issue_ids: ['LIN-4312'], resolution_hints: 'Issue may be too complex for single attempt' },
    { id: 3, pattern_hash: 'g7h8i9', error_type: 'build_failure', error_message: 'TypeScript compilation error', first_seen: daysAgo(14), last_seen: daysAgo(3), occurrence_count: 3, sources: ['github', 'linear'], example_issue_ids: ['GH-290'], resolution_hints: 'Verify type definitions match' },
  ],
  activity_last_hour: {
    issue_detected: 2,
    agent_spawn: 2,
    pr_created: 1,
    pr_merged: 1,
  },
  metric_counts_last_24h: {
    processing_time_secs: 42,
    inference_duration_ms: 42,
    pr_review_time_mins: 18,
  },
  diagnostics: {
    db_size_mb: 24.6,
    embedding_count: 847,
    model: 'nomic-embed-text-v1.5',
  },
  pr_analytics: {
    total: 15,
    open: 3,
    merged: 10,
    closed: 2,
    avg_time_to_first_review_mins: 72,
    avg_time_to_merge_mins: 348,
    avg_review_cycles: 1.2,
    merge_rate: 66.7,
    by_repo: {
      'acme/api-gateway': 5,
      'acme/web-frontend': 4,
      'acme/billing-service': 3,
      'acme/mobile-app': 2,
      'acme/shared-utils': 1,
    },
    avg_time_to_pr_mins: 3.1,
    rejection_reasons: [
      { category: 'Test failures', count: 3 },
      { category: 'Code style', count: 2 },
    ],
  },
  agent_spawns_today: 14,
  agent_spawns_this_week: 68,
};

export const telemetryTimeseries = {
  period: 'day',
  bucket_minutes: 60,
  generated_at: iso(now),
  points: Array.from({ length: 24 }, (_, i) => ({
    bucket_start: ago(23 - i),
    total: Math.floor(1 + Math.random() * 4),
    pending: i === 23 ? 1 : 0,
    success: Math.floor(1 + Math.random() * 3),
    failed: Math.random() > 0.8 ? 1 : 0,
    merged: Math.floor(Math.random() * 2),
    closed: 0,
    cannot_fix: 0,
  })),
};

export const telemetryPipeline = {
  generated_at: iso(now),
  period: 'day',
  totals: {
    fetched: 156,
    matched: 48,
    queued: 42,
    processed: 42,
    pr_created: 37,
    retries_found: 2,
    retries_executed: 2,
    retries_failed: 0,
    pr_status_checks: 84,
    pr_status_merged: 22,
    pr_status_closed: 1,
    pr_status_errors: 0,
    regression_watches_created: 22,
    auto_resolved_on_merge: 18,
    cascade_triggered: 3,
    cascade_failed: 0,
  },
  conversion: {
    match_rate: 30.8,
    queue_rate: 87.5,
    processing_rate: 100,
    pr_yield_rate: 88.1,
  },
  poll_load: {
    poll_cycles: 288,
    avg_cycle_secs: 4.2,
    p95_cycle_secs: 12.8,
    active_avg: 1.4,
    active_max: 3,
    pending_avg: 0.8,
    pending_max: 4,
    total_latest: 247,
  },
  per_source: [
    { source: 'linear', fetched: 68, matched: 22, queued: 20, processed: 20, pr_created: 18, retries_executed: 1, retries_failed: 0, match_rate: 32.4, queue_rate: 90.9, processing_rate: 100, pr_yield_rate: 90 },
    { source: 'sentry', fetched: 45, matched: 14, queued: 12, processed: 12, pr_created: 11, retries_executed: 1, retries_failed: 0, match_rate: 31.1, queue_rate: 85.7, processing_rate: 100, pr_yield_rate: 91.7 },
    { source: 'github', fetched: 28, matched: 8, queued: 7, processed: 7, pr_created: 6, retries_executed: 0, retries_failed: 0, match_rate: 28.6, queue_rate: 87.5, processing_rate: 100, pr_yield_rate: 85.7 },
    { source: 'discord', fetched: 15, matched: 4, queued: 3, processed: 3, pr_created: 2, retries_executed: 0, retries_failed: 0, match_rate: 26.7, queue_rate: 75, processing_rate: 100, pr_yield_rate: 66.7 },
  ],
};

export const telemetryLatency = {
  generated_at: iso(now),
  period: 'day',
  overall: { samples: 42, avg_secs: 174, p50_secs: 155, p95_secs: 395, p99_secs: 710, max_secs: 890 },
  by_status: [
    { status: 'success', summary: { samples: 37, avg_secs: 168, p50_secs: 148, p95_secs: 380, p99_secs: 680, max_secs: 820 } },
    { status: 'failed', summary: { samples: 3, avg_secs: 312, p50_secs: 290, p95_secs: 410, p99_secs: 410, max_secs: 410 } },
    { status: 'merged', summary: { samples: 22, avg_secs: 158, p50_secs: 140, p95_secs: 345, p99_secs: 620, max_secs: 720 } },
  ],
  histogram: [
    { label: '<1m', upper_bound_secs: 60, count: 2 },
    { label: '1-2m', upper_bound_secs: 120, count: 8 },
    { label: '2-3m', upper_bound_secs: 180, count: 14 },
    { label: '3-5m', upper_bound_secs: 300, count: 10 },
    { label: '5-10m', upper_bound_secs: 600, count: 5 },
    { label: '10-15m', upper_bound_secs: 900, count: 2 },
    { label: '>15m', upper_bound_secs: null, count: 1 },
  ],
};

// ── Repos ───────────────────────────────────────────────────────────────

export const repos = [
  { id: 1, name: 'acme/api-gateway', path: '/home/claudear/repos/api-gateway', scm_url: 'https://github.com/acme/api-gateway', default_branch: 'main', file_count: 1247, last_indexed_at: ago(1), created_at: daysAgo(60) },
  { id: 2, name: 'acme/web-frontend', path: '/home/claudear/repos/web-frontend', scm_url: 'https://github.com/acme/web-frontend', default_branch: 'main', file_count: 2310, last_indexed_at: ago(1), created_at: daysAgo(60) },
  { id: 3, name: 'acme/billing-service', path: '/home/claudear/repos/billing-service', scm_url: 'https://github.com/acme/billing-service', default_branch: 'main', file_count: 891, last_indexed_at: ago(1), created_at: daysAgo(45) },
  { id: 4, name: 'acme/mobile-app', path: '/home/claudear/repos/mobile-app', scm_url: 'https://github.com/acme/mobile-app', default_branch: 'develop', file_count: 1834, last_indexed_at: ago(2), created_at: daysAgo(45) },
  { id: 5, name: 'acme/shared-utils', path: '/home/claudear/repos/shared-utils', scm_url: 'https://github.com/acme/shared-utils', default_branch: 'main', file_count: 342, last_indexed_at: ago(1), created_at: daysAgo(90) },
];

export const repoStats = {
  repo_count: 5,
  file_count: 6624,
  last_indexed_at: ago(1),
};

export const dependencies = [
  { id: 1, upstream: 'acme/shared-utils', downstream: 'acme/api-gateway', dep_type: 'npm', created_at: daysAgo(60) },
  { id: 2, upstream: 'acme/shared-utils', downstream: 'acme/web-frontend', dep_type: 'npm', created_at: daysAgo(60) },
  { id: 3, upstream: 'acme/shared-utils', downstream: 'acme/billing-service', dep_type: 'npm', created_at: daysAgo(45) },
  { id: 4, upstream: 'acme/api-gateway', downstream: 'acme/web-frontend', dep_type: 'api', created_at: daysAgo(30) },
];

// ── Repo Learning ───────────────────────────────────────────────────────

export const repoLearning = {
  repo: 'acme/api-gateway',
  knowledge: [
    {
      key: 'common_fix_patterns',
      label: 'Common Fix Patterns',
      entries: [
        { id: 1, value: 'Add null checks before accessing nested properties', source_type: 'merge_analysis', confidence: 0.92, occurrence_count: 14, updated_at: daysAgo(1) },
        { id: 2, value: 'Use try/catch blocks around external API calls', source_type: 'merge_analysis', confidence: 0.88, occurrence_count: 11, updated_at: daysAgo(2) },
        { id: 3, value: 'Add request timeout configuration to HTTP clients', source_type: 'merge_analysis', confidence: 0.85, occurrence_count: 8, updated_at: daysAgo(3) },
      ],
    },
    {
      key: 'testing_patterns',
      label: 'Testing Patterns',
      entries: [
        { id: 4, value: 'Use Jest with supertest for API endpoint testing', source_type: 'review_pattern', confidence: 0.95, occurrence_count: 22, updated_at: daysAgo(1) },
        { id: 5, value: 'Mock external services with nock', source_type: 'review_pattern', confidence: 0.90, occurrence_count: 16, updated_at: daysAgo(2) },
      ],
    },
  ],
  knowledge_total: 5,
  instructions: [
    { id: 1, repo: 'acme/api-gateway', source_type: 'review_pattern', instruction_text: 'Always add integration tests for new API endpoints', occurrence_count: 18, confidence: 0.94, is_active: true, created_at: daysAgo(20), updated_at: daysAgo(2) },
    { id: 2, repo: 'acme/api-gateway', source_type: 'review_pattern', instruction_text: 'Use zod for request validation schemas', occurrence_count: 12, confidence: 0.91, is_active: true, created_at: daysAgo(15), updated_at: daysAgo(3) },
  ],
  review_patterns: [
    { id: 1, scm_repo: 'acme/api-gateway', category: 'error_handling', pattern_text: 'Wrap async handlers in try/catch', example_comments: ['Please add error handling here', 'This should catch the async error'], occurrence_count: 14, promoted_to_instruction: true, created_at: daysAgo(30), updated_at: daysAgo(2) },
    { id: 2, scm_repo: 'acme/api-gateway', category: 'testing', pattern_text: 'Add test for edge case', example_comments: ['Can you add a test for empty input?', 'Missing test for null case'], occurrence_count: 11, promoted_to_instruction: false, created_at: daysAgo(25), updated_at: daysAgo(5) },
  ],
  review_pattern_summary: { total_patterns: 2, by_category: { error_handling: 1, testing: 1 }, promoted_count: 1 },
  strategies: [
    { id: 1, attempt_id: 247, files_explored: ['src/routes/search.ts', 'src/utils/pagination.ts', 'test/routes/search.test.ts'], tests_run: 3, tools_used: { Read: 8, Edit: 3, Bash: 5 }, fix_approach: 'Modified pagination offset calculation', strategy_summary: 'Identified off-by-one error in pagination utility, fixed calculation and updated tests', fix_quality_score: 0.92, created_at: ago(0.5) },
  ],
  diff_analyses: [
    { id: 1, attempt_id: 247, pr_url: 'https://github.com/acme/api-gateway/pull/891', scm_repo: 'acme/api-gateway', pr_number: 891, files_changed: ['src/utils/pagination.ts', 'test/routes/search.test.ts'], file_types: { ts: 2 }, change_categories: ['bug_fix', 'test'], diff_summary: 'Fixed off-by-one pagination offset; added edge case tests', created_at: ago(0.5) },
  ],
  correlations: [
    { id: 1, repo_a: 'acme/api-gateway', repo_b: 'acme/web-frontend', correlation_count: 8, last_seen_at: daysAgo(1), window_hours: 48 },
    { id: 2, repo_a: 'acme/api-gateway', repo_b: 'acme/shared-utils', correlation_count: 5, last_seen_at: daysAgo(3), window_hours: 48 },
  ],
};

// ── Feedback ────────────────────────────────────────────────────────────

export const feedback = [
  { id: 1, attempt_id: 246, source: 'sentry', issue_id: 'SENTRY-90142', issue_text: 'Null pointer in payment webhook handler', prompt_used: 'Fix the null pointer exception...', outcome: 'merged', error_type: null, learnings: 'Added null check before accessing nested payment.metadata.customer_id', keywords: ['null-pointer', 'webhook', 'payment'] },
  { id: 2, attempt_id: 245, source: 'github', issue_id: 'GH-312', issue_text: 'Update deprecated crypto API', prompt_used: 'Update deprecated crypto.createCipher...', outcome: 'merged', error_type: null, learnings: 'Replaced createCipher with createCipheriv using random IV', keywords: ['crypto', 'deprecation', 'security'] },
  { id: 3, attempt_id: 240, source: 'sentry', issue_id: 'SENTRY-88901', issue_text: 'Memory leak in event stream', prompt_used: 'Fix the memory leak...', outcome: 'failed', error_type: 'test_failure', learnings: 'EventSource cleanup requires removing all listeners before closing', keywords: ['memory-leak', 'event-stream', 'cleanup'] },
  { id: 4, attempt_id: 243, source: 'discord', issue_id: 'DISC-78', issue_text: 'Mobile nav not closing', prompt_used: 'Fix the mobile navigation...', outcome: 'merged', error_type: null, learnings: 'Added useEffect cleanup with route change listener', keywords: ['mobile', 'navigation', 'react'] },
  { id: 5, attempt_id: 241, source: 'linear', issue_id: 'LIN-4521', issue_text: 'Auth token refresh race condition', prompt_used: 'Fix the auth token refresh...', outcome: 'merged', error_type: null, learnings: 'Used mutex lock pattern to prevent concurrent token refresh', keywords: ['auth', 'race-condition', 'token'] },
];

// ── Inference ───────────────────────────────────────────────────────────

export const inferenceStats = {
  total_attempts: 247,
  with_feedback: 89,
  correct: 82,
  accuracy: 92.1,
  by_confidence: { high: 178, medium: 52, low: 14, none: 3 },
};

export const inferenceHistory = Array.from({ length: 20 }, (_, i) => ({
  id: i + 1,
  issue_id: issueIds[i % issueIds.length],
  issue_source: issueSources[i % issueSources.length],
  extracted_keywords: ['auth', 'pagination', 'webhook', 'memory', 'crypto'][i % 5],
  inferred_repo_name: prRepos[i % 5],
  confidence: ['high', 'high', 'high', 'medium', 'high'][i % 5],
  inference_reason: 'Stack trace and file path analysis',
  was_correct: i % 7 !== 0 ? true : false,
  duration_ms: 120 + Math.floor(Math.random() * 80),
  created_at: daysAgo(20 - i),
}));

// ── Config ──────────────────────────────────────────────────────────────

export const config = {
  content: `workspace = "~/.claudear/repos"
known_orgs = ["acme"]
auto_discover_paths = ["~/projects"]
poll_interval_ms = 300000
max_concurrent = 2

[linear]
api_key = "lin_api_****"
trigger_labels = ["bug", "claudear"]

[sentry]
auth_token = "sntrys_****"
org_slug = "acme"
min_events = 5

[github]
token = "ghp_****"

[notifications.discord]
webhook_url = "https://discord.com/api/webhooks/****"
channel_id = "1234567890"

[notifications.slack]
channel = "#claudear-alerts"

[claude]
model = "sonnet"
skip_permissions = true
`,
  path: '/home/claudear/claudear.toml',
};

// ── Sources ─────────────────────────────────────────────────────────────

export const sources = {
  sources: [
    { name: 'linear', enabled: true, config: { team: 'ENG', labels: ['bug', 'claudear'] } },
    { name: 'sentry', enabled: true, config: { org: 'acme', min_events: 5 } },
    { name: 'github', enabled: true, config: { orgs: ['acme'] } },
    { name: 'discord', enabled: true, config: { channel_id: '1234567890' } },
  ],
};

// ── Regressions ─────────────────────────────────────────────────────────

export const regressions = [
  { id: 1, issue_type: 'linear', issue_id: 'LIN-4892', fix_attempt_id: 247, status: 'monitoring', pr_merged_at: null, monitoring_started_at: ago(0.5), resolved_at: null, regressed_at: null, created_at: ago(0.5) },
  { id: 2, issue_type: 'sentry', issue_id: 'SENTRY-90142', fix_attempt_id: 246, status: 'monitoring', pr_merged_at: ago(2), monitoring_started_at: ago(2), resolved_at: null, regressed_at: null, created_at: ago(2) },
  { id: 3, issue_type: 'github', issue_id: 'GH-312', fix_attempt_id: 245, status: 'resolved', pr_merged_at: ago(28), monitoring_started_at: ago(28), resolved_at: ago(4), regressed_at: null, created_at: ago(28) },
  { id: 4, issue_type: 'linear', issue_id: 'LIN-4521', fix_attempt_id: 241, status: 'resolved', pr_merged_at: ago(36), monitoring_started_at: ago(36), resolved_at: ago(12), regressed_at: null, created_at: ago(36) },
];

// ── Experiments ──────────────────────────────────────────────────────────

export const experiments = [
  { id: 1, experiment_name: 'prompt_v2', variant: 'structured_cot', prompt_template: 'Think step by step...', prompt_hash: 'abc123', created_at: daysAgo(14), active: true, success_count: 42, failure_count: 5, avg_time_to_merge: 320, avg_review_score: 4.2 },
  { id: 2, experiment_name: 'prompt_v2', variant: 'baseline', prompt_template: 'Fix the following issue...', prompt_hash: 'def456', created_at: daysAgo(14), active: false, success_count: 38, failure_count: 8, avg_time_to_merge: 380, avg_review_score: 3.8 },
];

// ── Errors ───────────────────────────────────────────────────────────────

export const errors = [
  { id: 1, pattern_hash: 'a1b2c3', error_type: 'test_failure', error_message: 'Test suite assertion failure', first_seen: daysAgo(30), last_seen: ago(14), occurrence_count: 8, sources: ['linear', 'sentry'], example_issue_ids: ['LIN-4510', 'SENTRY-88901'], resolution_hints: 'Check test fixtures and mock data' },
  { id: 2, pattern_hash: 'd4e5f6', error_type: 'timeout', error_message: 'Claude process timed out after 6h', first_seen: daysAgo(21), last_seen: daysAgo(5), occurrence_count: 4, sources: ['linear'], example_issue_ids: ['LIN-4312'], resolution_hints: 'Issue may be too complex' },
  { id: 3, pattern_hash: 'g7h8i9', error_type: 'build_failure', error_message: 'TypeScript compilation error', first_seen: daysAgo(14), last_seen: daysAgo(3), occurrence_count: 3, sources: ['github', 'linear'], example_issue_ids: ['GH-290'], resolution_hints: 'Verify type definitions' },
];

// ── Attempts ────────────────────────────────────────────────────────────

export const attempts = {
  attempts: overview.recent_attempts,
  total: 247,
  page: 1,
  per_page: 20,
};

// ── Users ───────────────────────────────────────────────────────────────

export const users = [
  { id: 1, email: 'admin@acme.dev', name: 'Admin', role: 'admin', avatar_url: null, created_at: daysAgo(90), updated_at: daysAgo(1) },
  { id: 2, email: 'jake@acme.dev', name: 'Jake', role: 'viewer', avatar_url: null, created_at: daysAgo(60), updated_at: daysAgo(5) },
];

const deployQaTip = (
  id: number,
  track: string,
  repo: string,
  tag: string,
  status: 'pending' | 'running' | 'verified' | 'unverified' | 'failed' | 'errored',
  hours: number,
  attemptId: number | null,
) => ({
  id,
  track,
  repo,
  tag,
  status,
  published_at: ago(hours),
  release_url: `https://github.com/${repo}/releases/tag/${tag}`,
  author_login: 'octocat',
  attempt_id: attemptId,
  discord_message_url: null,
  discord_thread_url: null,
  created_at: ago(hours),
  updated_at: ago(Math.max(hours - 0.5, 0)),
});

export const deployQaTips = {
  tips: [
    deployQaTip(6, 'api', 'acme/api-gateway', '1.43.12', 'running', 0.2, 251),
    deployQaTip(5, 'web', 'acme/web-console', '2.8.4', 'verified', 3, 249),
    deployQaTip(4, 'api', 'acme/api-gateway', '1.43.11', 'verified', 26, 244),
    deployQaTip(3, 'web', 'acme/web-console', '2.8.3', 'failed', 50, 238),
    deployQaTip(2, 'api', 'acme/api-gateway', '1.43.10', 'unverified', 74, 231),
  ],
};

export const deployQaStatus = {
  enabled: true,
  restart_required: false,
  poll_interval_ms: 60_000,
  timeout_secs: 1_800,
  skip_if_previous_running: true,
  discord_channel_id: '1203948576102938475',
  tracks: [
    { name: 'api', repo: 'acme/api-gateway', tag_filter: 'not_suffix:-rc', last_seen: deployQaTips.tips[0], in_flight: true },
    { name: 'web', repo: 'acme/web-console', tag_filter: 'any', last_seen: deployQaTips.tips[1], in_flight: false },
  ],
};

export const deployQaConfig = {
  settings: {
    enabled: true,
    poll_interval_ms: 60_000,
    skip_if_previous_running: true,
    timeout_secs: 1_800,
    discord_channel_id: '1203948576102938475',
    github_discord_map_path: null,
    instructions_path: 'qa/instructions.md',
    tracks: [
      { name: 'api', repo: 'acme/api-gateway', tag_filter: 'not_suffix:-rc' },
      { name: 'web', repo: 'acme/web-console', tag_filter: 'any' },
    ],
  },
  tag_filter_kinds: [
    { kind: 'any', requires_value: false },
    { kind: 'suffix', requires_value: true },
    { kind: 'not_suffix', requires_value: true },
  ],
  track_name_separator: ':',
  restart_required: false,
  writable: true,
  revision: 'a1b2c3d4',
};

const feature = (
  key: string,
  label: string,
  description: string,
  category: 'source' | 'scm' | 'notifier' | 'automation' | 'intelligence' | 'platform',
  configured: boolean,
  enabled: boolean,
  settings: Array<{ name: string; value: string | number | boolean | null }> = [],
  secrets: Array<{ name: string; set: boolean }> = [],
) => ({ key, label, description, category, configured, enabled, settings, secrets });

export const features = {
  features: [
    feature('issues.linear', 'Linear', 'Fix issues assigned in Linear', 'source', true, true, [{ name: 'team', value: 'ENG' }], [{ name: 'api_key', set: true }]),
    feature('issues.sentry', 'Sentry', 'Fix new Sentry errors', 'source', true, true, [{ name: 'organization', value: 'acme' }], [{ name: 'auth_token', set: true }]),
    feature('issues.jira', 'Jira', 'Fix issues assigned in Jira', 'source', false, false),
    feature('scm.github', 'GitHub', 'Open and track pull requests', 'scm', true, true, [{ name: 'owner', value: 'acme' }], [{ name: 'token', set: true }]),
    feature('notifiers.discord', 'Discord notifier', 'Post attempt updates to Discord', 'notifier', true, true, [], [{ name: 'bot_token', set: true }]),
    feature('notifiers.slack', 'Slack notifier', 'Post attempt updates to Slack', 'notifier', false, false),
    feature('deploy_qa', 'Deploy QA', 'QA every new release tag', 'automation', true, true, [{ name: 'tracks', value: 2 }]),
    feature('llm', 'Local LLM', 'Local models for chat and triage', 'intelligence', true, false),
    feature('mcp_server', 'MCP server', 'Expose claudear tools over MCP', 'platform', true, true),
  ],
};
