import { Sentry } from './sentry'

const API_BASE = '/api';

export function getWsBase(): string {
  const loc = window.location;
  const proto = loc.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${proto}//${loc.host}`;
}


export interface Stats {
  total: number;
  pending: number;
  success: number;
  failed: number;
  merged: number;
  closed: number;
  cannot_fix: number;
  by_source: Record<string, SourceStats>;
}

export interface SourceStats {
  total: number;
  success: number;
  failed: number;
  merged: number;
  closed: number;
  cannot_fix: number;
}

export interface AttemptSummary {
  id: number;
  source: string;
  short_id: string;
  title: string;
  status: string;
  pr_url: string | null;
  attempted_at: string;
  retry_count: number;
}

export interface SourceSummary {
  name: string;
  total: number;
  success: number;
  failed: number;
  merged: number;
  success_rate: number;
}

export interface Overview {
  stats: Stats;
  success_rate: number;
  merge_rate: number;
  recent_attempts: AttemptSummary[];
  sources: SourceSummary[];
  time_savings?: TimeSavings | null;
  agent_spawns_today?: number;
}

export interface AttemptsResponse {
  attempts: AttemptSummary[];
  total: number;
  page: number;
  per_page: number;
}

export interface IssueSummary {
  id: number;
  source: string;
  issue_id: string;
  short_id: string | null;
  title: string | null;
  description: string | null;
  url: string | null;
  priority: string | null;
  status: string | null;
  labels: string[] | null;
  has_embedding: boolean;
  created_at: string;
  updated_at: string | null;
}

export interface IssuesResponse {
  issues: IssueSummary[];
  total: number;
  page: number;
  per_page: number;
}

export interface Health {
  status: string;
  version: string;
  uptime_secs: number;
  database: {
    status: string;
    error?: string;
  };
}

export interface RetriesResponse {
  retryable: AttemptSummary[];
  ready: AttemptSummary[];
  max_retries: number;
}

export interface SourceInfo {
  name: string;
  enabled: boolean;
  config: Record<string, unknown>;
}

export interface SourcesResponse {
  sources: SourceInfo[];
}


export type Role = 'admin' | 'viewer'

export const ADMIN_ROLE: Role = 'admin'

export interface AuthUser {
  id: number
  email: string
  name: string
  role: Role
  avatar_url: string | null
}

export interface LoginResponse {
  user: AuthUser
}

export type UserStatus = 'active' | 'invited'

export interface UserRecord {
  id: number
  email: string
  name: string
  role: Role
  avatar_url: string | null
  status: UserStatus
  invite_expires_at: string | null
  created_at: string
  updated_at: string
}

export interface UserInvite {
  invite_link: string
  invite_url: string | null
  expires_at: string
}

export interface CreatedUser extends UserRecord {
  invite: UserInvite | null
}

export interface InviteDetails {
  name: string
  email: string
  expires_at: string
}

export type IdentityOrigin = 'console' | 'config' | 'unset'

export interface Identities {
  linear_names: string[]
  github_usernames: string[]
  sentry_usernames: string[]
  jira_usernames: string[]
  gitlab_usernames: string[]
  discord_id: string | null
  slack_id: string | null
  email: string | null
  push_user_key: string | null
  sms_number: string | null
  whatsapp_number: string | null
  telegram_chat_id: string | null
}

export interface UserIdentities {
  user_id: number
  name: string
  login_email: string | null
  person_key: string | null
  slug: string | null
  origin: IdentityOrigin
  status: UserStatus
  redacted: boolean
  identities: Identities
  updated_at: string | null
}

export type IdentityFieldName = 'slug' | keyof Identities

export interface IdentityViolation {
  field: IdentityFieldName
  message: string
  email?: string
}

export type IdentitiesUpdate = { slug?: string } & {
  [K in keyof Identities]?: Identities[K] extends string[] ? string[] : string
}

export const SENSITIVE_IDENTITY_FIELDS: ReadonlyArray<keyof Identities> = [
  'email',
  'push_user_key',
  'sms_number',
  'whatsapp_number',
  'telegram_chat_id',
]


export interface ActivityLogEntry {
  id: number;
  timestamp: string;
  activity_type: string;
  source: string | null;
  issue_id: string | null;
  short_id: string | null;
  message: string;
  metadata: Record<string, unknown> | null;
}

export interface ClaudeExecution {
  id: number;
  attempt_id: number | null;
  started_at: string;
  completed_at: string | null;
  duration_secs: number | null;
  exit_code: number | null;
  timed_out: boolean;
  stdout_preview: string | null;
  stderr_preview: string | null;
  stdout_log_path: string | null;
  stderr_log_path: string | null;
  event_log_path?: string | null;
  prompt_used: string | null;
  prompt_hash: string | null;
  model_version: string | null;
  working_directory: string | null;
  git_branch: string | null;
  git_commit_before: string | null;
  git_commit_after: string | null;
  files_changed: number | null;
  lines_added: number | null;
  lines_removed: number | null;
}

export interface PrReviewRecord {
  id: number;
  attempt_id: number | null;
  pr_url: string;
  reviewer: string | null;
  review_state: string | null;
  submitted_at: string | null;
  body: string | null;
  sentiment: string | null;
  actionable_feedback: string | null;
}

export interface FixAttemptDetail {
  id: number;
  issue_id: string;
  short_id: string;
  source: string;
  attempted_at: string;
  pr_url: string | null;
  scm_repo: string | null;
  scm_pr_number: number | null;
  status: string;
  error_message: string | null;
  merged_at: string | null;
  resolved_at: string | null;
  retry_count: number;
  last_retry_at: string | null;
  issue_labels?: string[];
  parent_attempt_id: number | null;
  cascade_repo: string | null;
}

export interface AttemptDetailResponse {
  attempt: FixAttemptDetail;
  executions: ClaudeExecution[];
  reviews: PrReviewRecord[];
  feedback: FixOutcome | null;
}

export interface AttemptExecutionLogResponse {
  attempt_id: number;
  execution_id: number;
  stream: 'stdout' | 'stderr' | 'events';
  path: string | null;
  content: string | null;
  truncated: boolean;
}

export interface RetrievalUsageRecord {
  id: number | null;
  attempt_id: number;
  source_kind: string; // 'code_chunk' | 'similar_issue' | 'discord_chunk' | 'qa'
  chunk_ref: string;
  file_path: string | null;
  rank: number;
  similarity_score: number;
  injected: boolean;
  char_len: number | null;
  quality_score: number | null;
}

export interface AttemptRetrievalResponse {
  attempt_id: number;
  rows: RetrievalUsageRecord[];
}

export interface AnalyticsSummary {
  success_rate: number;
  total_processed: number;
  total_successful: number;
  total_merged: number;
  avg_processing_time_secs: number | null;
  avg_time_to_merge_hours: number | null;
  most_common_error: string | null;
  success_rate_by_source: Record<string, number>;
  avg_time_to_pr_mins?: number | null;
  cost_estimate?: CostEstimate | null;
  mttr_trend?: MttrDataPoint[];
  repo_leaderboard?: RepoLeaderboardEntry[];
  commit_trend?: CommitTrendPoint[];
  support_rating?: SupportRatingSummary | null;
}

export interface CommitTrendPoint {
  period_start: string;
  commits: number;
}

export interface SupportReply {
  action_run_id: number;
  source: string;
  short_id: string;
  kind: string;
  detail?: string | null;
  created_at: string;
  response_time_mins?: number | null;
  rating?: number | null;
  note?: string | null;
  rated_by?: string | null;
}

export type SupportDraftStatus = 'pending' | 'approved' | 'rejected' | 'sending' | 'sent' | 'failed';

export interface SupportDraftMessage {
  author: string;
  poster: boolean;
  content: string;
  timestamp: string;
}

export interface SupportDraft {
  thread_id: string;
  title: string;
  url: string;
  status: SupportDraftStatus;
  answer: string;
  reviewer?: string | null;
  error?: string | null;
  updated_at: string;
  messages: SupportDraftMessage[];
}

export interface SupportRatingSummary {
  total_replies: number;
  rated_count: number;
  avg_rating: number | null;
  avg_response_time_mins: number | null;
  avg_rating_by_source: Record<string, number>;
}

export interface ProcessingMetric {
  id: number;
  timestamp: string;
  metric_name: string;
  metric_value: number;
  source: string | null;
  tags: Record<string, unknown> | null;
}

export interface ErrorPattern {
  id: number;
  pattern_hash: string;
  error_type: string | null;
  error_message: string | null;
  first_seen: string;
  last_seen: string;
  occurrence_count: number;
  sources: string[] | null;
  example_issue_ids: string[] | null;
  resolution_hints: string | null;
}

export interface PrRecord {
  id: number;
  pr_url: string;
  scm_repo: string;
  pr_number: number;
  attempt_id: number | null;
  issue_id: string | null;
  issue_source: string | null;
  title: string | null;
  description: string | null;
  author: string | null;
  head_branch: string | null;
  base_branch: string | null;
  status: string;
  created_at: string;
  updated_at: string | null;
  merged_at: string | null;
  closed_at: string | null;
  approvals_count: number;
  changes_requested_count: number;
  comments_count: number;
  last_review_at: string | null;
  time_to_first_review_mins: number | null;
  time_to_merge_mins: number | null;
  review_cycles: number;
  files_changed: number | null;
  lines_added: number | null;
  lines_removed: number | null;
}

export interface PrAnalytics {
  total: number;
  open: number;
  merged: number;
  closed: number;
  avg_time_to_first_review_mins: number | null;
  avg_time_to_merge_mins: number | null;
  avg_review_cycles: number | null;
  merge_rate: number | null;
  by_repo: Record<string, number>;
  avg_time_to_pr_mins?: number | null;
  rejection_reasons?: RejectionReason[];
}

export interface RejectionReason {
  category: string;
  count: number;
}

export interface CostEstimate {
  total_cost: number;
  avg_cost_per_fix: number;
  fix_count: number;
  cost_source: string;
  period: string;
}

export interface MttrDataPoint {
  period_start: string;
  mttr_minutes: number;
  sample_count: number;
}

export interface RepoLeaderboardEntry {
  repo: string;
  total: number;
  success_rate: number;
  merge_rate: number;
  avg_time_to_merge_mins: number | null;
}

export interface TimeSavings {
  merged_count: number;
  hours_saved: number;
  cost_saved: number;
  period: string;
}

export interface FixOutcome {
  id: number;
  attempt_id: number;
  source: string;
  issue_id: string;
  issue_text: string;
  prompt_used: string;
  outcome: string;
  error_type: string | null;
  learnings: string | null;
  keywords: string[];
}

export interface RegressionWatch {
  id: number;
  issue_type: string;
  issue_id: string;
  fix_attempt_id: number;
  status: string;
  pr_merged_at: string | null;
  monitoring_started_at: string | null;
  resolved_at: string | null;
  regressed_at: string | null;
  created_at: string;
}

export interface RegressionCheck {
  id: number;
  regression_watch_id: number;
  issue_still_exists: boolean;
  checked_at: string | null;
  check_details: string | null;
  created_at: string;
}

export interface PromptExperiment {
  id: number;
  experiment_name: string;
  variant: string;
  prompt_template: string;
  prompt_hash: string;
  created_at: string;
  active: boolean;
  success_count: number;
  failure_count: number;
  avg_time_to_merge: number | null;
  avg_review_score: number | null;
}

export interface CreatePromptExperimentRequest {
  experiment_name: string;
  variant: string;
  prompt_template: string;
  active?: boolean;
}

export interface StoredIndexedRepo {
  id: number;
  name: string;
  path: string;
  scm_url: string | null;
  default_branch: string;
  file_count: number;
  last_indexed_at: string;
  created_at: string;
}

export interface IndexStats {
  repo_count: number;
  file_count: number;
  last_indexed_at: string | null;
}

export interface StoredDependency {
  id: number;
  upstream: string;
  downstream: string;
  dep_type: string;
  created_at: string;
}

export interface IndexingProgress {
  status: string;
  total_repos: number;
  indexed_repos: number;
  current_repo: string | null;
  current_repo_files: number;
  total_files_indexed: number;
  started_at: string | null;
  updated_at: string | null;
}

export interface StoredDiscordChannel {
  channel_id: string;
  guild_id: string | null;
  parent_id: string | null;
  category_name: string | null;
  name: string | null;
  kind: string;
  archived: boolean;
  backfill_complete: boolean;
  last_indexed_message_id: string | null;
  last_indexed_at: string | null;
  chunk_count: number;
  indexed_from: string | null;
  indexed_to: string | null;
}

export interface DiscordKnowledgebaseStats {
  channel_count: number;
  thread_count: number;
  chunk_count: number;
  last_indexed_at: string | null;
}

export interface InferenceStats {
  total_attempts: number;
  with_feedback: number;
  correct: number;
  accuracy: number;
  by_confidence: {
    high: number;
    medium: number;
    low: number;
    none: number;
  };
}

export interface InferenceHistoryEntry {
  id: number;
  issue_id: string;
  issue_source: string;
  extracted_keywords: string | null;
  inferred_repo_name: string | null;
  confidence: string | null;
  inference_reason: string | null;
  was_correct: boolean | null;
  duration_ms: number | null;
  created_at: string;
}

export interface TelemetryWindowMetric {
  window: string;
  processed: number;
  successful: number;
  failed: number;
  merged: number;
  success_rate: number;
  error_rate: number;
  throughput_per_hour: number;
}

export interface TelemetryQueueMetrics {
  pending_attempts: number;
  retryable_attempts: number;
  ready_retries: number;
  open_prs: number;
  watches_awaiting_release: number;
  watches_monitoring: number;
  watches_resolved: number;
  watches_regressed: number;
}

export interface ProcessingTimeSummary {
  samples: number;
  avg_secs: number | null;
  p50_secs: number | null;
  p95_secs: number | null;
  p99_secs: number | null;
  max_secs: number | null;
}

export interface TelemetryProcessingTime {
  all_time: ProcessingTimeSummary;
  last_24h: ProcessingTimeSummary;
}

export interface SourceTelemetry {
  source: string;
  total: number;
  pending: number;
  success: number;
  failed: number;
  merged: number;
  closed: number;
  cannot_fix: number;
  retryable: number;
  success_rate: number;
}

export interface TelemetryTimeseriesPoint {
  bucket_start: string;
  total: number;
  pending: number;
  success: number;
  failed: number;
  merged: number;
  closed: number;
  cannot_fix: number;
}

export interface TelemetryOverview {
  generated_at: string;
  uptime_secs: number;
  windows: TelemetryWindowMetric[];
  queue: TelemetryQueueMetrics;
  processing_time: TelemetryProcessingTime;
  source_breakdown: SourceTelemetry[];
  top_errors: ErrorPattern[];
  activity_last_hour: Record<string, number>;
  metric_counts_last_24h: Record<string, number>;
  diagnostics?: Record<string, unknown> | null;
  pr_analytics: PrAnalytics;
  agent_spawns_today?: number;
  agent_spawns_this_week?: number;
}

export interface TelemetryTimeseries {
  period: string;
  bucket_minutes: number;
  generated_at: string;
  points: TelemetryTimeseriesPoint[];
}

export interface TelemetryPipelineTotals {
  fetched: number;
  matched: number;
  queued: number;
  processed: number;
  pr_created: number;
  retries_found: number;
  retries_executed: number;
  retries_failed: number;
  pr_status_checks: number;
  pr_status_merged: number;
  pr_status_closed: number;
  pr_status_errors: number;
  regression_watches_created: number;
  auto_resolved_on_merge: number;
  cascade_triggered: number;
  cascade_failed: number;
}

export interface TelemetryPipelineConversion {
  match_rate: number | null;
  queue_rate: number | null;
  processing_rate: number | null;
  pr_yield_rate: number | null;
}

export interface TelemetryPollLoad {
  poll_cycles: number;
  avg_cycle_secs: number | null;
  p95_cycle_secs: number | null;
  active_avg: number | null;
  active_max: number | null;
  pending_avg: number | null;
  pending_max: number | null;
  total_latest: number | null;
}

export interface TelemetryPipelineSource {
  source: string;
  fetched: number;
  matched: number;
  queued: number;
  processed: number;
  pr_created: number;
  retries_executed: number;
  retries_failed: number;
  match_rate: number | null;
  queue_rate: number | null;
  processing_rate: number | null;
  pr_yield_rate: number | null;
}

export interface TelemetryPipeline {
  generated_at: string;
  period: string;
  totals: TelemetryPipelineTotals;
  conversion: TelemetryPipelineConversion;
  poll_load: TelemetryPollLoad;
  per_source: TelemetryPipelineSource[];
}

export interface TelemetryLatencyByStatus {
  status: string;
  summary: ProcessingTimeSummary;
}

export interface TelemetryLatencyHistogramBucket {
  label: string;
  upper_bound_secs: number | null;
  count: number;
}

export interface TelemetryLatency {
  generated_at: string;
  period: string;
  overall: ProcessingTimeSummary;
  by_status: TelemetryLatencyByStatus[];
  histogram: TelemetryLatencyHistogramBucket[];
}

let onUnauthorized: (() => void) | null = null

export function setOnUnauthorized(cb: () => void) {
  onUnauthorized = cb
}

/**
 * Read the CSRF token from the readable `claudear_csrf` cookie. The backend
 * sets this cookie on every request and requires mutating requests (POST/PUT/
 * DELETE) to echo it back in the `x-csrf-token` header (double-submit cookie
 * pattern). Returns an empty string if the cookie isn't present yet.
 */
export function getCsrfToken(): string {
  const match = document.cookie.match(/(?:^|;\s*)claudear_csrf=([^;]*)/)
  return match ? decodeURIComponent(match[1]) : ''
}

export class ApiError extends Error {
  readonly status: number
  readonly code?: string
  readonly violations: IdentityViolation[]

  constructor(status: number, message: string, code?: string, violations: IdentityViolation[] = []) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
    this.violations = violations
  }
}

type ApiMethod = 'GET' | 'POST' | 'PUT' | 'DELETE'

const failureVerbs: Record<ApiMethod, string> = {
  GET: 'fetch',
  POST: 'post',
  PUT: 'put',
  DELETE: 'delete',
}

async function readApiError(res: Response, url: string, method: ApiMethod): Promise<ApiError> {
  let body: unknown
  try {
    body = await res.json()
  } catch {
    body = undefined
  }
  const payload = typeof body === 'object' && body !== null
    ? (body as { error?: unknown; code?: unknown; violations?: unknown })
    : {}
  const message = typeof payload.error === 'string' && payload.error
    ? payload.error
    : `Failed to ${failureVerbs[method]} ${url}: ${res.status}`
  const code = typeof payload.code === 'string' ? payload.code : undefined
  const violations = Array.isArray(payload.violations) ? (payload.violations as IdentityViolation[]) : []
  return new ApiError(res.status, message, code, violations)
}

async function ensureOk(res: Response, url: string, method: ApiMethod): Promise<void> {
  if (res.status === 401) {
    onUnauthorized?.()
    throw new Error('Unauthorized')
  }
  if (!res.ok) {
    const err = await readApiError(res, url, method)
    Sentry.captureException(err, { tags: { 'api.url': url, 'api.method': method, 'api.status': String(res.status) } })
    throw err
  }
}

export async function fetchJson<T>(url: string): Promise<T> {
  const res = await fetch(url)
  await ensureOk(res, url, 'GET')
  return res.json()
}

async function postJson<T>(url: string, body?: unknown): Promise<T> {
  Sentry.addBreadcrumb({ category: 'api', message: `POST ${url}`, level: 'info' })
  const res = await fetch(url, {
    method: 'POST',
    headers: {
      'x-csrf-token': getCsrfToken(),
      ...(body ? { 'Content-Type': 'application/json' } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  })
  await ensureOk(res, url, 'POST')
  if (res.status === 204) return undefined as T
  return res.json()
}

async function putJson<T>(url: string, body: unknown): Promise<T> {
  Sentry.addBreadcrumb({ category: 'api', message: `PUT ${url}`, level: 'info' })
  const res = await fetch(url, {
    method: 'PUT',
    headers: {
      'Content-Type': 'application/json',
      'x-csrf-token': getCsrfToken(),
    },
    body: JSON.stringify(body),
  })
  await ensureOk(res, url, 'PUT')
  return res.json()
}

async function sendJson<T>(method: ApiMethod, url: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {}
  if (method !== 'GET') {
    Sentry.addBreadcrumb({ category: 'api', message: `${method} ${url}`, level: 'info' })
    headers['x-csrf-token'] = getCsrfToken()
  }
  if (body !== undefined) headers['Content-Type'] = 'application/json'
  const res = await fetch(url, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  await ensureOk(res, url, method)
  if (res.status === 204) return undefined as T
  return res.json()
}

export async function deleteRequest(url: string): Promise<void> {
  Sentry.addBreadcrumb({ category: 'api', message: `DELETE ${url}`, level: 'info' })
  const res = await fetch(url, {
    method: 'DELETE',
    headers: { 'x-csrf-token': getCsrfToken() },
  })
  await ensureOk(res, url, 'DELETE')
}

export async function fetchOverview(): Promise<Overview> {
  return fetchJson(`${API_BASE}/stats/overview`);
}

export async function getStats(): Promise<Stats> {
  return fetchJson(`${API_BASE}/stats`);
}

export async function fetchAttempts(params?: {
  status?: string;
  source?: string;
  page?: number;
  per_page?: number;
}): Promise<AttemptsResponse> {
  const searchParams = new URLSearchParams();
  if (params?.status) searchParams.set('status', params.status);
  if (params?.source) searchParams.set('source', params.source);
  if (params?.page) searchParams.set('page', params.page.toString());
  if (params?.per_page) searchParams.set('per_page', params.per_page.toString());
  return fetchJson(`${API_BASE}/attempts?${searchParams}`);
}

export async function fetchIssues(params?: {
  source?: string;
  page?: number;
  per_page?: number;
}): Promise<IssuesResponse> {
  const searchParams = new URLSearchParams();
  if (params?.source) searchParams.set('source', params.source);
  if (params?.page) searchParams.set('page', params.page.toString());
  if (params?.per_page) searchParams.set('per_page', params.per_page.toString());
  return fetchJson(`${API_BASE}/issues?${searchParams}`);
}

export async function getHealth(): Promise<Health> {
  return fetchJson(`${API_BASE}/health`);
}

export async function getRetries(): Promise<RetriesResponse> {
  return fetchJson(`${API_BASE}/retries`);
}

export async function getSources(): Promise<SourcesResponse> {
  return fetchJson(`${API_BASE}/sources`);
}

export async function fetchActivity(params?: {
  limit?: number;
  source?: string;
}): Promise<ActivityLogEntry[]> {
  const searchParams = new URLSearchParams();
  if (params?.limit) searchParams.set('limit', params.limit.toString());
  if (params?.source) searchParams.set('source', params.source);
  return fetchJson(`${API_BASE}/activity?${searchParams}`);
}

export interface TimelineEvent {
  time: string;
  event: string;
  message?: string;
  // Flattened metadata context (e.g. `repo`, `pr`).
  [key: string]: unknown;
}

export interface IssueTimeline {
  issue: string;
  events: TimelineEvent[];
}

export async function fetchIssueTimeline(
  source: string,
  issueId: string,
): Promise<IssueTimeline> {
  return fetchJson(
    `${API_BASE}/issues/${encodeURIComponent(source)}/${encodeURIComponent(issueId)}/timeline`,
  );
}

export async function fetchAttemptDetail(attemptId: number): Promise<AttemptDetailResponse> {
  return fetchJson(`${API_BASE}/attempts/${attemptId}/detail`);
}

export async function fetchAttemptRetrieval(
  attemptId: number,
): Promise<AttemptRetrievalResponse> {
  return fetchJson(`${API_BASE}/attempts/${attemptId}/retrieval`);
}

export async function fetchAttemptExecutionLog(
  attemptId: number,
  executionId: number,
  stream: 'stdout' | 'stderr' | 'events',
): Promise<AttemptExecutionLogResponse> {
  return fetchJson(
    `${API_BASE}/attempts/${attemptId}/logs/${executionId}/${stream}`,
  );
}

export async function fetchAnalyticsSummary(): Promise<AnalyticsSummary> {
  return fetchJson(`${API_BASE}/analytics/summary`);
}

export async function fetchMetrics(params?: {
  name?: string;
  period?: string;
  limit?: number;
}): Promise<ProcessingMetric[]> {
  const searchParams = new URLSearchParams();
  if (params?.name) searchParams.set('name', params.name);
  if (params?.period) searchParams.set('period', params.period);
  if (params?.limit) searchParams.set('limit', params.limit.toString());
  return fetchJson(`${API_BASE}/metrics?${searchParams}`);
}

export async function fetchErrors(limit = 50): Promise<ErrorPattern[]> {
  return fetchJson(`${API_BASE}/errors?limit=${limit}`);
}

export async function fetchSupportReplies(): Promise<SupportReply[]> {
  return fetchJson(`${API_BASE}/support/replies`);
}

export async function rateReply(
  actionRunId: number,
  rating: number,
  note?: string,
): Promise<void> {
  await postJson(`${API_BASE}/support/replies/${actionRunId}/rating`, { rating, note });
}

export async function fetchSupportDrafts(): Promise<SupportDraft[]> {
  return fetchJson(`${API_BASE}/support/drafts`);
}

/** Review the draft revision (its `updated_at`) the reviewer saw; fails if it changed since. */
export async function reviewSupportDraft(
  threadId: string,
  revision: string,
  status: 'approved' | 'rejected',
  answer?: string,
): Promise<void> {
  await postJson(`${API_BASE}/support/drafts/${threadId}`, { revision, status, answer });
}

export async function fetchPrs(params?: {
  status?: string;
  limit?: number;
}): Promise<PrRecord[]> {
  const searchParams = new URLSearchParams();
  if (params?.status) searchParams.set('status', params.status);
  if (params?.limit) searchParams.set('limit', params.limit.toString());
  return fetchJson(`${API_BASE}/prs?${searchParams}`);
}

export async function fetchPrAnalytics(): Promise<PrAnalytics> {
  return fetchJson(`${API_BASE}/prs/analytics`);
}

export async function fetchFeedback(params?: {
  source?: string;
  limit?: number;
}): Promise<FixOutcome[]> {
  const searchParams = new URLSearchParams();
  if (params?.source) searchParams.set('source', params.source);
  if (params?.limit) searchParams.set('limit', params.limit.toString());
  return fetchJson(`${API_BASE}/feedback?${searchParams}`);
}

export async function fetchRegressions(status?: string): Promise<RegressionWatch[]> {
  const searchParams = new URLSearchParams();
  if (status) searchParams.set('status', status);
  return fetchJson(`${API_BASE}/regressions?${searchParams}`);
}

export async function fetchRegressionChecks(watchId: number): Promise<RegressionCheck[]> {
  return fetchJson(`${API_BASE}/regressions/${watchId}/checks`);
}

export async function fetchExperiments(): Promise<PromptExperiment[]> {
  return fetchJson(`${API_BASE}/experiments`);
}

export async function createExperiment(
  data: CreatePromptExperimentRequest,
): Promise<PromptExperiment> {
  return postJson(`${API_BASE}/experiments`, data);
}

export async function updateExperiment(
  id: number,
  data: CreatePromptExperimentRequest,
): Promise<{ ok: boolean }> {
  return putJson(`${API_BASE}/experiments/${id}`, data);
}

export async function fetchRepos(): Promise<StoredIndexedRepo[]> {
  return fetchJson(`${API_BASE}/repos`);
}

export async function fetchRepoStats(): Promise<IndexStats> {
  return fetchJson(`${API_BASE}/repos/stats`);
}

export async function fetchDependencies(): Promise<StoredDependency[]> {
  return fetchJson(`${API_BASE}/repos/dependencies`);
}

export async function fetchDiscordChannels(): Promise<StoredDiscordChannel[]> {
  return fetchJson(`${API_BASE}/channels`);
}

export async function fetchDiscordChannelStats(): Promise<DiscordKnowledgebaseStats> {
  return fetchJson(`${API_BASE}/channels/stats`);
}

export const INDEXING_PROGRESS_WS_PATH = '/api/repos/indexing-progress';

export async function fetchInferenceStats(): Promise<InferenceStats> {
  return fetchJson(`${API_BASE}/inference/stats`);
}

export async function fetchInferenceHistory(limit = 50): Promise<InferenceHistoryEntry[]> {
  return fetchJson(`${API_BASE}/inference/history?limit=${limit}`);
}

export async function fetchTelemetryOverview(): Promise<TelemetryOverview> {
  return fetchJson(`${API_BASE}/telemetry/overview`);
}

export async function fetchTelemetryTimeseries(params?: {
  period?: string;
  bucket_minutes?: number;
}): Promise<TelemetryTimeseries> {
  const searchParams = new URLSearchParams();
  if (params?.period) searchParams.set('period', params.period);
  if (params?.bucket_minutes) {
    searchParams.set('bucket_minutes', params.bucket_minutes.toString());
  }
  return fetchJson(`${API_BASE}/telemetry/timeseries?${searchParams}`);
}

export async function fetchTelemetryPipeline(params?: {
  period?: string;
}): Promise<TelemetryPipeline> {
  const searchParams = new URLSearchParams();
  if (params?.period) searchParams.set('period', params.period);
  return fetchJson(`${API_BASE}/telemetry/pipeline?${searchParams}`);
}

export async function fetchTelemetryLatency(params?: {
  period?: string;
}): Promise<TelemetryLatency> {
  const searchParams = new URLSearchParams();
  if (params?.period) searchParams.set('period', params.period);
  return fetchJson(`${API_BASE}/telemetry/latency?${searchParams}`);
}


export interface KnowledgeEntry {
  id: number
  value: string
  source_type: string
  confidence: number
  occurrence_count: number
  updated_at: string
}

export interface KnowledgeGroup {
  key: string
  label: string
  entries: KnowledgeEntry[]
}

export interface ReviewPatternSummary {
  total_patterns: number
  by_category: Record<string, number>
  promoted_count: number
}

export interface ReviewPattern {
  id: number
  scm_repo: string
  category: string
  pattern_text: string
  example_comments: string[]
  occurrence_count: number
  promoted_to_instruction: boolean
  created_at: string
  updated_at: string
}

export interface PromotedInstruction {
  id: number
  repo: string
  source_type: string
  instruction_text: string
  occurrence_count: number
  confidence: number
  is_active: boolean
  created_at: string
  updated_at: string
}

export interface StrategyFingerprint {
  id: number
  attempt_id: number
  files_explored: string[]
  tests_run: number
  tools_used: Record<string, number>
  fix_approach: string
  strategy_summary: string
  fix_quality_score: number | null
  created_at: string
}

export interface DiffAnalysisSummary {
  id: number
  attempt_id: number
  pr_url: string
  scm_repo: string
  pr_number: number
  files_changed: string[]
  file_types: Record<string, number>
  change_categories: string[]
  diff_summary: string
  created_at: string
}

export interface CrossRepoCorrelation {
  id: number
  repo_a: string
  repo_b: string
  correlation_count: number
  last_seen_at: string
  window_hours: number
}

export interface RepoLearningResponse {
  repo: string
  knowledge: KnowledgeGroup[]
  knowledge_total: number
  instructions: PromotedInstruction[]
  review_patterns: ReviewPattern[]
  review_pattern_summary: ReviewPatternSummary
  strategies: StrategyFingerprint[]
  diff_analyses: DiffAnalysisSummary[]
  correlations: CrossRepoCorrelation[]
}

export async function fetchRepoLearning(repo: string): Promise<RepoLearningResponse> {
  return fetchJson(`${API_BASE}/repos/${encodeURIComponent(repo)}/learning`)
}


export type ConfigErrorCode = 'invalid' | 'read_only' | 'revision_mismatch' | 'unreadable'

export interface OptionalConfigSection {
  key: string
  template: string
}

export interface ConfigResponse {
  content: string
  secret_paths: string[]
  runtime_overrides: string[]
  restart_required: boolean
  writable: boolean
  revision: string
  optional_sections: OptionalConfigSection[]
}

export interface SaveConfigResponse {
  ok: true
  restart_required: boolean
  revision: string
  message: string
}

export async function fetchConfig(): Promise<ConfigResponse> {
  return fetchJson(`${API_BASE}/config`)
}

export async function saveConfig(content: string, revision?: string): Promise<SaveConfigResponse> {
  return putJson(`${API_BASE}/config`, revision === undefined ? { content } : { content, revision })
}


export interface InstructionResponse {
  scope: string
  repo: string | null
  text: string
  updated_at: string | null
}

export async function fetchGlobalInstruction(): Promise<InstructionResponse> {
  return fetchJson(`${API_BASE}/instructions/global`)
}

export async function saveGlobalInstruction(text: string): Promise<{ ok: boolean }> {
  return putJson(`${API_BASE}/instructions/global`, { text })
}

export async function fetchRepoInstruction(repo: string): Promise<InstructionResponse> {
  return fetchJson(`${API_BASE}/repos/${encodeURIComponent(repo)}/instructions`)
}

export async function saveRepoInstruction(repo: string, text: string): Promise<{ ok: boolean }> {
  return putJson(`${API_BASE}/repos/${encodeURIComponent(repo)}/instructions`, { text })
}


export async function login(email: string, password: string): Promise<LoginResponse> {
  return postJson(`${API_BASE}/auth/login`, { email, password })
}

export async function logout(): Promise<void> {
  await postJson(`${API_BASE}/auth/logout`)
}

export async function getMe(): Promise<AuthUser> {
  return fetchJson(`${API_BASE}/auth/me`)
}

export async function updateProfile(data: {
  name?: string; password?: string; current_password?: string
}): Promise<UserRecord> {
  return putJson(`${API_BASE}/auth/profile`, data)
}

export async function uploadAvatar(file: File): Promise<{ avatar_url: string }> {
  const form = new FormData()
  form.append('avatar', file)
  const res = await fetch(`${API_BASE}/auth/avatar`, { method: 'POST', body: form })
  if (res.status === 401) {
    onUnauthorized?.()
    throw new Error('Unauthorized')
  }
  if (!res.ok) throw new Error(`Failed to upload avatar: ${res.status}`)
  return res.json()
}


export async function fetchUsers(): Promise<UserRecord[]> {
  return sendJson('GET', `${API_BASE}/users`)
}

export async function getUser(id: number): Promise<UserRecord> {
  return sendJson('GET', `${API_BASE}/users/${id}`)
}

export async function createUser(data: {
  email: string; name: string; role: Role; password?: string; identities?: IdentitiesUpdate
}): Promise<CreatedUser> {
  return sendJson('POST', `${API_BASE}/users`, data)
}

export async function updateUser(id: number, data: {
  email?: string; password?: string; name?: string; role?: Role
}): Promise<UserRecord> {
  return sendJson('PUT', `${API_BASE}/users/${id}`, data)
}

export async function deleteUser(id: number): Promise<void> {
  await sendJson<unknown>('DELETE', `${API_BASE}/users/${id}`)
}

export async function issueInvite(userId: number): Promise<UserInvite> {
  return sendJson('POST', `${API_BASE}/users/${userId}/invite`)
}

export async function revokeInvite(userId: number): Promise<void> {
  await sendJson<unknown>('DELETE', `${API_BASE}/users/${userId}/invite`)
}

export async function inspectInvite(token: string): Promise<InviteDetails> {
  return sendJson('POST', `${API_BASE}/auth/invite/inspect`, { token })
}

export async function acceptInvite(token: string, password: string): Promise<void> {
  await sendJson<unknown>('POST', `${API_BASE}/auth/invite/accept`, { token, password })
}

export function inviteAbsoluteUrl(invite: UserInvite): string {
  return invite.invite_url ?? window.location.origin + invite.invite_link
}

export async function fetchIdentities(): Promise<UserIdentities[]> {
  return sendJson('GET', `${API_BASE}/identities`)
}

export async function fetchUserIdentities(userId: number): Promise<UserIdentities> {
  return sendJson('GET', `${API_BASE}/users/${userId}/identities`)
}

export async function updateUserIdentities(userId: number, update: IdentitiesUpdate): Promise<UserIdentities> {
  return sendJson('PUT', `${API_BASE}/users/${userId}/identities`, update)
}

export async function resetUserIdentities(userId: number): Promise<UserIdentities> {
  return sendJson('DELETE', `${API_BASE}/users/${userId}/identities`)
}

/** A personal access token (never includes the secret after creation). */
export interface ApiToken {
  id: string
  user_id: number
  name: string
  token_prefix: string
  created_at: string
  last_used_at: string | null
  expires_at: string | null
}

/** Admin view: a token joined with its owner. */
export interface ApiTokenWithOwner extends ApiToken {
  user_email: string
  user_name: string
}

/** Response when creating a token: the row plus the one-time plaintext secret. */
export interface CreatedToken extends ApiToken {
  secret: string
}

/** List the tokens belonging to a specific user (admin). */
export async function fetchUserTokens(userId: number): Promise<ApiToken[]> {
  return fetchJson(`${API_BASE}/users/${userId}/tokens`)
}

/** Mint a token for a specific user (admin). The secret is returned once. */
export async function createUserToken(
  userId: number,
  data: { name: string; expires_at?: string },
): Promise<CreatedToken> {
  return postJson(`${API_BASE}/users/${userId}/tokens`, data)
}

/** Revoke a token by id. */
export async function revokeToken(id: string): Promise<void> {
  return deleteRequest(`${API_BASE}/tokens/${id}`)
}

/** Current user's own tokens (self-serve). */
export async function fetchMyTokens(): Promise<ApiToken[]> {
  return fetchJson(`${API_BASE}/tokens`)
}

/** Mint a token for the current user. `expires_at` is RFC3339; the secret is returned once. */
export async function createMyToken(data: { name: string; expires_at?: string }): Promise<CreatedToken> {
  return postJson(`${API_BASE}/tokens`, data)
}

/** Admin: every token with its owner. */
export async function fetchAllTokens(): Promise<ApiTokenWithOwner[]> {
  return fetchJson(`${API_BASE}/tokens/all`)
}

export const SECURITY_REPORT_STATES = [
  'received',
  'assessing',
  'needs_human',
  'registering',
  'spam',
  'replying',
  'closed',
  'tracking',
  'resolved',
] as const
export type SecurityReportState = (typeof SECURITY_REPORT_STATES)[number]

export const SECURITY_OUTCOMES = [
  'spam',
  'valid',
  'not_reproducible',
  'already_fixed',
  'not_vulnerability',
  'out_of_scope',
  'duplicate',
] as const
export type SecurityOutcome = (typeof SECURITY_OUTCOMES)[number]
export type SecurityOverrideOutcome = Exclude<SecurityOutcome, 'valid'>

export const VULNERABILITY_STATES = ['awaiting_fix', 'fixing', 'merged', 'live', 'needs_human'] as const
export type VulnerabilityState = (typeof VULNERABILITY_STATES)[number]

export const SECURITY_SEVERITIES = ['critical', 'high', 'medium', 'low', 'informational'] as const
export type SecuritySeverity = (typeof SECURITY_SEVERITIES)[number]

export const SECURITY_REPLY_KINDS = [
  'confirmed',
  'not_reproducible',
  'already_fixed',
  'not_vulnerability',
  'out_of_scope',
  'duplicate',
  'fix_live',
  'fix_live_rewarded',
] as const
export type SecurityReplyKind = (typeof SECURITY_REPLY_KINDS)[number]

export const DELIVERY_STATUSES = ['sending', 'sent', 'failed', 'unconfirmed'] as const
export type DeliveryStatus = (typeof DELIVERY_STATUSES)[number]

export const AWARD_STATUSES = ['prepared', 'owner_notified', 'awarded', 'cancelled'] as const
export type AwardStatus = (typeof AWARD_STATUSES)[number]

export const SECURITY_EVENT_KINDS = [
  'received',
  'assessment_started',
  'assessment_released',
  'assessment_failed',
  'needs_human',
  'overridden',
  'admin_confirmed',
  'screened_spam',
  'assessed',
  'vulnerability_created',
  'duplicate_linked',
  'first_reporter_replaced',
  'reply_claimed',
  'reply_sent',
  'reply_failed',
  'reply_unconfirmed',
  'delivery_confirmed',
  'conversation_closed',
  'reporter_follow_up',
  'fix_started',
  'pull_request_opened',
  'pull_request_linked',
  'fix_failed',
  'merged',
  'cloud_bump_required',
  'live',
  'deployment_check_failed',
  'award_prepared',
  'owner_notified',
  'awarded',
  'award_cancelled',
] as const
export type SecurityEventKind = (typeof SECURITY_EVENT_KINDS)[number]

export const SECURITY_REVIEW_REASONS = [
  'assessment_failed',
  'assessment_inconclusive',
  'unverifiable_fix',
  'attachment_evidence',
  'untracked_repository',
  'unindexed_repository',
  'pull_request_closed',
  'fix_failed',
  'reply_unconfirmed',
  'deployment_check_failed',
  'fix_found_in_history',
  'reply_failed',
  'missing_merge_commit',
  'registration_failed',
  'invalid_pull_request',
] as const
export type SecurityReviewReason = (typeof SECURITY_REVIEW_REASONS)[number]

export interface SecuritySummary {
  reports_by_state: Partial<Record<SecurityReportState, number>>
  reports_by_outcome: Partial<Record<SecurityOutcome, number>>
  vulnerabilities_by_state: Partial<Record<VulnerabilityState, number>>
  pending_awards: number
  unconfirmed_replies: number
}

export interface SecurityReportRow {
  id: number
  short_id: string
  url: string
  subject: string
  reporter_email: string | null
  reported_at: string
  state: SecurityReportState
  outcome: SecurityOutcome | null
  vulnerability_id: number | null
  is_first_reporter: boolean
  vulnerability_state: VulnerabilityState | null
  severity: SecuritySeverity | null
  pull_request_url: string | null
  deployed_version: string | null
  live_at: string | null
  award_status: AwardStatus | null
  unconfirmed_reply: boolean
  updated_at: string
}

export interface SecurityReport {
  id: number
  source: string
  conversation_id: string
  short_id: string
  mailbox_id: string
  url: string
  subject: string
  reporter_email: string | null
  reporter_customer_id: string | null
  reported_at: string
  state: SecurityReportState
  outcome: SecurityOutcome | null
  public_reason: string | null
  assessment_summary: string | null
  affected_repository: string | null
  fixed_by_commit: string | null
  fixed_on: string | null
  spam_confidence: number | null
  assessment_attempts: number
  customer_thread_count: number
  vulnerability_id: number | null
  created_at: string
  updated_at: string
}

export interface SecurityVulnerability {
  id: number
  title: string
  summary: string
  repository: string | null
  severity: SecuritySeverity | null
  state: VulnerabilityState
  first_report_id: number
  first_reported_at: string
  fix_attempt_id: number | null
  pull_request_url: string | null
  merge_commit: string | null
  merged_at: string | null
  deployed_version: string | null
  live_at: string | null
  created_at: string
  updated_at: string
}

export interface SecurityEvent {
  id: number
  report_id: number | null
  vulnerability_id: number | null
  kind: SecurityEventKind
  from_state: string | null
  to_state: string | null
  actor: string | null
  detail: unknown
  created_at: string
}

export interface SecurityReply {
  id: number
  report_id: number
  kind: SecurityReplyKind
  status: DeliveryStatus
  error: string | null
  claimed_at: string
  completed_at: string | null
}

export interface SecurityAward {
  id: number
  vulnerability_id: number
  report_id: number
  recipient_email: string | null
  campaign: string
  credits: number
  status: AwardStatus
  actor: string | null
  note: string | null
  prepared_at: string
  updated_at: string
}

export interface SecurityReportDetail {
  report: SecurityReport
  vulnerability: SecurityVulnerability | null
  events: SecurityEvent[]
  replies: SecurityReply[]
  award: SecurityAward | null
  related_reports: SecurityReportRow[]
}

export type SecurityOverride =
  | { action: 'reassess' }
  | {
      action: 'outcome'
      outcome: SecurityOverrideOutcome
      public_reason?: string
      fixed_on?: string
    }
  | { action: 'confirm_delivery'; kind: SecurityReplyKind; delivered: boolean }
  | {
      action: 'confirm_valid'
      title: string
      summary: string
      repository?: string
      severity?: SecuritySeverity
    }

export interface SecurityReportFilter {
  state?: SecurityReportState
  outcome?: SecurityOutcome
  limit?: number
}

export async function fetchSecuritySummary(): Promise<SecuritySummary> {
  return fetchJson(`${API_BASE}/security/summary`)
}

export async function fetchSecurityReports(filter?: SecurityReportFilter): Promise<SecurityReportRow[]> {
  const searchParams = new URLSearchParams()
  if (filter?.state) searchParams.set('state', filter.state)
  if (filter?.outcome) searchParams.set('outcome', filter.outcome)
  if (filter?.limit) searchParams.set('limit', filter.limit.toString())
  return fetchJson(`${API_BASE}/security/reports?${searchParams}`)
}

export async function fetchSecurityReport(id: number): Promise<SecurityReportDetail> {
  return fetchJson(`${API_BASE}/security/reports/${id}`)
}

export async function overrideSecurityReport(
  id: number,
  override: SecurityOverride,
): Promise<SecurityReport> {
  return postJson(`${API_BASE}/security/reports/${id}/override`, override)
}

export async function markSecurityAwardGranted(
  vulnerabilityId: number,
  note?: string,
): Promise<SecurityAward> {
  return postJson(`${API_BASE}/security/vulnerabilities/${vulnerabilityId}/award`, note ? { note } : {})
}

export async function linkSecurityPullRequest(
  vulnerabilityId: number,
  pullRequestUrl: string,
): Promise<SecurityVulnerability> {
  return postJson(`${API_BASE}/security/vulnerabilities/${vulnerabilityId}/pull-request`, {
    pull_request_url: pullRequestUrl,
  })
}

export async function markSecurityLive(
  vulnerabilityId: number,
  deployedVersion: string,
): Promise<SecurityVulnerability> {
  return postJson(`${API_BASE}/security/vulnerabilities/${vulnerabilityId}/live`, {
    deployed_version: deployedVersion,
  })
}

export type DeployQaTipStatus = 'pending' | 'running' | 'verified' | 'unverified' | 'failed' | 'errored'

export interface DeployQaTipView {
  id: number
  track: string
  repo: string
  tag: string
  status: DeployQaTipStatus
  published_at: string | null
  release_url: string | null
  author_login: string | null
  attempt_id: number | null
  discord_message_url: string | null
  discord_thread_url: string | null
  created_at: string
  updated_at: string
}

export interface DeployQaTrackStatus {
  name: string
  repo: string
  tag_filter: string
  last_seen: DeployQaTipView | null
  in_flight: boolean
}

export interface DeployQaStatus {
  enabled: boolean
  restart_required: boolean
  poll_interval_ms: number
  timeout_secs: number
  skip_if_previous_running: boolean
  discord_channel_id: string | null
  tracks: DeployQaTrackStatus[]
}

export interface DeployQaTrackSettings {
  name: string
  repo: string
  tag_filter: string
}

export interface DeployQaSettings {
  enabled: boolean
  poll_interval_ms: number
  skip_if_previous_running: boolean
  timeout_secs: number
  discord_channel_id: string | null
  github_discord_map_path: string | null
  instructions_path: string | null
  tracks: DeployQaTrackSettings[]
}

export interface DeployQaTagFilterKind {
  kind: string
  requires_value: boolean
}

export interface DeployQaConfigResponse {
  settings: DeployQaSettings
  tag_filter_kinds: DeployQaTagFilterKind[]
  track_name_separator: string
  restart_required: boolean
  writable: boolean
  revision: string
}

export interface DeployQaSaveResponse {
  ok: true
  restart_required: boolean
  revision: string
  message: string
}

export async function fetchDeployQaStatus(): Promise<DeployQaStatus> {
  return fetchJson(`${API_BASE}/deploy-qa`)
}

export async function fetchDeployQaTips(track?: string): Promise<DeployQaTipView[]> {
  const query = track ? `?${new URLSearchParams({ track })}` : ''
  const response = await fetchJson<{ tips: DeployQaTipView[] }>(`${API_BASE}/deploy-qa/tips${query}`)
  return response.tips
}

export async function fetchDeployQaConfig(): Promise<DeployQaConfigResponse> {
  return fetchJson(`${API_BASE}/deploy-qa/config`)
}

export async function saveDeployQaConfig(
  settings: DeployQaSettings,
  revision: string,
): Promise<DeployQaSaveResponse> {
  return putJson(`${API_BASE}/deploy-qa/config`, { settings, revision })
}

export type FeatureCategory = 'source' | 'scm' | 'notifier' | 'automation' | 'intelligence' | 'platform'

export type FeatureSettingValue =
  | string
  | number
  | boolean
  | null
  | FeatureSettingValue[]
  | { [name: string]: FeatureSettingValue }

export interface FeatureSetting {
  name: string
  value: FeatureSettingValue
}

export interface FeatureSecret {
  name: string
  set: boolean
}

export interface FeatureStatus {
  key: string
  label: string
  description: string
  category: FeatureCategory
  configured: boolean
  enabled: boolean
  settings: FeatureSetting[]
  secrets: FeatureSecret[]
}

export interface FeaturesResponse {
  features: FeatureStatus[]
}

export async function fetchFeatures(): Promise<FeatureStatus[]> {
  const response = await fetchJson<FeaturesResponse>(`${API_BASE}/features`)
  return response.features
}
