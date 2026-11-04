import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import * as SWR from "swr";

type FetchCall = { url: string; method: string };
type PrefetchRouteData = typeof import("../src/lib/route-prefetch").prefetchRouteData;
type ResetPrefetchedRoutes = typeof import("../src/lib/route-prefetch").resetPrefetchedRoutes;

let prefetchRouteData: PrefetchRouteData | null = null;
let resetPrefetchedRoutes: ResetPrefetchedRoutes | null = null;

function response(data: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: status >= 200 && status < 300 ? "OK" : "Error",
    json: () => Promise.resolve(data),
    text: () => Promise.resolve(JSON.stringify(data)),
    headers: new Headers({ "content-type": "application/json" }),
  };
}

function mockPrefetchPayload(requestUrl: string): unknown {
  if (requestUrl.includes("/api/config")) {
    throw new Error("config should be handled by explicit reject branch");
  }
  if (requestUrl.includes("/api/stats/overview")) {
    return {
      stats: {
        total: 0,
        pending: 0,
        success: 0,
        failed: 0,
        merged: 0,
        closed: 0,
        cannot_fix: 0,
        by_source: {},
      },
      success_rate: 0,
      merge_rate: 0,
      recent_attempts: [],
      sources: [],
      time_savings: null,
      agent_spawns_today: 0,
    };
  }
  if (requestUrl.includes("/api/retries")) {
    return { retryable: [], ready: [], max_retries: 3 };
  }
  if (requestUrl.includes("/api/issues?")) {
    return { issues: [], total: 0, page: 1, per_page: 100 };
  }
  if (requestUrl.includes("/api/attempts?")) {
    return { attempts: [], total: 0, page: 1, per_page: 20 };
  }
  if (requestUrl.includes("/api/prs/analytics")) {
    return {
      total: 0,
      open: 0,
      merged: 0,
      closed: 0,
      avg_time_to_first_review_mins: null,
      avg_time_to_merge_mins: null,
      avg_review_cycles: null,
      merge_rate: null,
      by_repo: {},
    };
  }
  if (requestUrl.includes("/api/analytics/summary")) {
    return {
      success_rate: 0,
      total_processed: 0,
      total_successful: 0,
      total_merged: 0,
      avg_processing_time_secs: null,
      avg_time_to_merge_hours: null,
      most_common_error: null,
      success_rate_by_source: {},
    };
  }
  if (requestUrl.includes("/api/metrics?")) return [];
  if (requestUrl.includes("/api/errors?")) return [];
  if (requestUrl.includes("/api/feedback?")) return [];
  if (requestUrl.includes("/api/regressions?")) return [];
  if (requestUrl.includes("/api/experiments")) return [];
  if (requestUrl.includes("/api/repos/stats")) {
    return { repo_count: 0, file_count: 0, last_indexed_at: null };
  }
  if (requestUrl.includes("/api/repos/dependencies")) return [];
  if (requestUrl.endsWith("/api/repos")) return [];
  if (requestUrl.includes("/api/inference/stats")) {
    return {
      total_requests: 0,
      total_tokens_in: 0,
      total_tokens_out: 0,
      total_cost: 0,
      models: [],
      by_provider: {},
      by_status: {},
      average_latency_ms: null,
      p95_latency_ms: null,
    };
  }
  if (requestUrl.includes("/api/inference/history?")) return [];
  if (requestUrl.includes("/api/activity?")) return [];
  if (requestUrl.includes("/api/telemetry/overview")) {
    return {
      totals: {
        requests: 0,
        success: 0,
        errors: 0,
        tokens_in: 0,
        tokens_out: 0,
        cost_usd: 0,
      },
      providers: [],
      models: [],
      status_codes: [],
      periods: {},
    };
  }
  if (requestUrl.includes("/api/telemetry/timeseries")) {
    return { points: [], period: "hour", bucket_minutes: 5 };
  }
  if (requestUrl.includes("/api/telemetry/pipeline")) {
    return { stages: [], period: "hour" };
  }
  if (requestUrl.includes("/api/telemetry/latency")) {
    return { period: "hour", buckets: [], summary: { p50_ms: null, p95_ms: null, p99_ms: null } };
  }
  if (requestUrl.includes("/api/users")) return [];
  return { ok: true };
}

async function flushAsync() {
  await Promise.resolve();
  await Promise.resolve();
  await new Promise((resolve) => setTimeout(resolve, 0));
}

describe("prefetchRouteData", () => {
  const originalFetch = globalThis.fetch;
  let calls: FetchCall[] = [];

  beforeEach(async () => {
    if (!prefetchRouteData) {
      mock.module("swr", () => ({
        ...SWR,
        preload: <T>(_key: string | readonly unknown[], fetcher: () => Promise<T>) =>
          Promise.resolve().then(fetcher),
      }));

      ({ prefetchRouteData, resetPrefetchedRoutes } = await import("../src/lib/route-prefetch"));
    }

    resetPrefetchedRoutes!();
    calls = [];
    globalThis.fetch = mock((input: string | URL | Request, init?: RequestInit) => {
      const requestUrl =
        typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
      const method = init?.method ?? (input instanceof Request ? input.method : "GET");
      calls.push({ url: requestUrl, method });

      if (requestUrl.includes("/api/config")) {
        return Promise.reject(new Error("network error"));
      }

      return Promise.resolve(response(mockPrefetchPayload(requestUrl))) as Promise<Response>;
    }) as unknown as typeof fetch;
  });

  afterEach(() => {
    globalThis.fetch = originalFetch;
  });

  test("preloads critical routes, dedupes by route, and ignores unknown routes", async () => {
    expect(prefetchRouteData).toBeTruthy();
    prefetchRouteData!("/");
    prefetchRouteData!("/issues");
    prefetchRouteData!("/attempts");
    prefetchRouteData!("/prs");
    prefetchRouteData!("/analytics");
    prefetchRouteData!("/errors");
    prefetchRouteData!("/feedback");
    prefetchRouteData!("/regressions");
    prefetchRouteData!("/experiments");
    prefetchRouteData!("/learning");
    prefetchRouteData!("/repos");
    prefetchRouteData!("/inference");
    prefetchRouteData!("/activity");
    prefetchRouteData!("/telemetry");
    prefetchRouteData!("/config", "admin");
    prefetchRouteData!("/users", "admin");
    prefetchRouteData!("/does-not-exist");

    await flushAsync();

    const initialUserCalls = calls.filter((call) => call.url.includes("/api/users")).length;
    prefetchRouteData!("/users", "admin");
    prefetchRouteData!("/");
    prefetchRouteData!("/does-not-exist");
    await flushAsync();

    const userCallsAfterDuplicate = calls.filter((call) => call.url.includes("/api/users")).length;
    expect(userCallsAfterDuplicate).toBe(initialUserCalls);

    expect(calls.some((call) => call.url.includes("/api/stats/overview"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/retries"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/issues?"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/attempts?"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/prs/analytics"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/analytics/summary"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/metrics?"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/errors?limit=50"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/feedback?"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/experiments"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/repos/stats"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/repos/dependencies"))).toBe(true);
    expect(calls.some((call) => call.url.endsWith("/api/repos"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/inference/stats"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/inference/history?limit=50"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/activity?limit=200"))).toBe(true);
    expect(calls.some((call) => call.url.includes("/api/config"))).toBe(true);

    const prsCalls = calls.filter((call) => call.url.includes("/api/prs?"));
    expect(prsCalls.length).toBe(4);
    expect(prsCalls.some((call) => call.url.includes("status=open"))).toBe(true);
    expect(prsCalls.some((call) => call.url.includes("status=merged"))).toBe(true);
    expect(prsCalls.some((call) => call.url.includes("status=closed"))).toBe(true);

    const regressionCalls = calls.filter(
      (call) => call.url.includes("/api/regressions?") && !call.url.includes("/checks")
    );
    expect(regressionCalls.length).toBe(4);
    expect(regressionCalls.some((call) => call.url.includes("status=awaiting_release"))).toBe(true);
    expect(regressionCalls.some((call) => call.url.includes("status=monitoring"))).toBe(true);
    expect(regressionCalls.some((call) => call.url.includes("status=resolved"))).toBe(true);
    expect(regressionCalls.some((call) => call.url.includes("status=regressed"))).toBe(true);

    const telemetryOverviewCalls = calls.filter((call) => call.url.includes("/api/telemetry/overview"));
    const telemetryTimeseriesCalls = calls.filter((call) => call.url.includes("/api/telemetry/timeseries"));
    const telemetryPipelineCalls = calls.filter((call) => call.url.includes("/api/telemetry/pipeline"));
    const telemetryLatencyCalls = calls.filter((call) => call.url.includes("/api/telemetry/latency"));
    expect(telemetryOverviewCalls.length).toBe(1);
    expect(telemetryTimeseriesCalls.length).toBe(4);
    expect(telemetryPipelineCalls.length).toBe(4);
    expect(telemetryLatencyCalls.length).toBe(4);
    expect(telemetryTimeseriesCalls.some((call) => call.url.includes("period=hour"))).toBe(true);
    expect(telemetryTimeseriesCalls.some((call) => call.url.includes("bucket_minutes=360"))).toBe(true);
  });

  test("skips admin-only routes for viewers and anonymous users, then prefetches once an admin hovers", async () => {
    prefetchRouteData!("/config", "viewer");
    prefetchRouteData!("/users", "viewer");
    prefetchRouteData!("/config");
    prefetchRouteData!("/users");
    prefetchRouteData!("/", "viewer");
    await flushAsync();

    expect(calls.some((call) => call.url.includes("/api/config"))).toBe(false);
    expect(calls.some((call) => call.url.includes("/api/users"))).toBe(false);
    expect(calls.some((call) => call.url.includes("/api/stats/overview"))).toBe(true);

    prefetchRouteData!("/config", "admin");
    prefetchRouteData!("/users", "admin");
    await flushAsync();

    expect(calls.filter((call) => call.url.includes("/api/config")).length).toBe(1);
    expect(calls.filter((call) => call.url.includes("/api/users")).length).toBe(1);
  });
});
