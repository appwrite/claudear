import { afterEach, describe, expect, mock, test } from "bun:test";
import {
  ApiError,
  fetchSecurityReport,
  fetchSecurityReports,
  fetchSecuritySummary,
  linkSecurityPullRequest,
  markSecurityAwardGranted,
  markSecurityLive,
  overrideSecurityReport,
} from "../src/lib/api";

type Call = { url: string; method: string; body: unknown; headers: Record<string, string> };

function mockFetch(data: unknown, status = 200) {
  const calls: Call[] = [];
  globalThis.fetch = mock((input: string | URL | Request, init?: RequestInit) => {
    calls.push({
      url: String(input),
      method: init?.method ?? "GET",
      body: typeof init?.body === "string" ? JSON.parse(init.body) : undefined,
      headers: (init?.headers ?? {}) as Record<string, string>,
    });
    return Promise.resolve({
      ok: status >= 200 && status < 300,
      status,
      json: () => Promise.resolve(data),
    });
  }) as unknown as typeof fetch;
  return calls;
}

describe("security api", () => {
  const originalFetch = globalThis.fetch;

  afterEach(() => {
    globalThis.fetch = originalFetch;
  });

  test("fetchSecuritySummary reads the summary endpoint", async () => {
    const summary = {
      reports_by_state: { needs_human: 2 },
      reports_by_outcome: {},
      vulnerabilities_by_state: { live: 1 },
      pending_awards: 1,
      unconfirmed_replies: 0,
    };
    const calls = mockFetch(summary);
    expect(await fetchSecuritySummary()).toEqual(summary);
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe("/api/security/summary");
    expect(calls[0].method).toBe("GET");
  });

  test("fetchSecurityReports passes state, outcome and limit filters", async () => {
    const calls = mockFetch([]);
    await fetchSecurityReports({ state: "needs_human", outcome: "duplicate", limit: 250 });
    expect(calls[0].url).toBe("/api/security/reports?state=needs_human&outcome=duplicate&limit=250");
    expect(calls[0].method).toBe("GET");
  });

  test("fetchSecurityReports omits unset filters", async () => {
    const calls = mockFetch([]);
    await fetchSecurityReports();
    expect(calls[0].url).toBe("/api/security/reports?");
  });

  test("fetchSecurityReport reads one report", async () => {
    const calls = mockFetch({ report: { id: 42 } });
    await fetchSecurityReport(42);
    expect(calls[0].url).toBe("/api/security/reports/42");
  });

  test("overrideSecurityReport posts each override variant as the body", async () => {
    const calls = mockFetch({ id: 5 });
    await overrideSecurityReport(5, { action: "reassess" });
    await overrideSecurityReport(5, {
      action: "outcome",
      outcome: "already_fixed",
      fixed_on: "2026-10-01",
    });
    await overrideSecurityReport(5, { action: "confirm_delivery", kind: "confirmed", delivered: false });
    await overrideSecurityReport(5, {
      action: "confirm_valid",
      title: "Missing permission check",
      summary: "Team members can read other teams' files.",
      repository: "appwrite/appwrite",
      severity: "high",
    });

    expect(calls.map((call) => [call.url, call.method])).toEqual([
      ["/api/security/reports/5/override", "POST"],
      ["/api/security/reports/5/override", "POST"],
      ["/api/security/reports/5/override", "POST"],
      ["/api/security/reports/5/override", "POST"],
    ]);
    expect(calls.map((call) => call.body)).toEqual([
      { action: "reassess" },
      { action: "outcome", outcome: "already_fixed", fixed_on: "2026-10-01" },
      { action: "confirm_delivery", kind: "confirmed", delivered: false },
      {
        action: "confirm_valid",
        title: "Missing permission check",
        summary: "Team members can read other teams' files.",
        repository: "appwrite/appwrite",
        severity: "high",
      },
    ]);
    expect(calls[0].headers["Content-Type"]).toBe("application/json");
    expect("x-csrf-token" in calls[0].headers).toBe(true);
  });

  test("markSecurityAwardGranted posts the note, or an empty object without one", async () => {
    const calls = mockFetch({ id: 1, status: "awarded" });
    await markSecurityAwardGranted(9, "Coupon sent");
    await markSecurityAwardGranted(9);
    expect(calls[0].url).toBe("/api/security/vulnerabilities/9/award");
    expect(calls[0].method).toBe("POST");
    expect(calls[0].body).toEqual({ note: "Coupon sent" });
    expect(calls[1].body).toEqual({});
  });

  test("linkSecurityPullRequest posts the pull request url", async () => {
    const calls = mockFetch({ id: 9 });
    await linkSecurityPullRequest(9, "https://github.com/appwrite/appwrite/pull/123");
    expect(calls[0].url).toBe("/api/security/vulnerabilities/9/pull-request");
    expect(calls[0].method).toBe("POST");
    expect(calls[0].body).toEqual({ pull_request_url: "https://github.com/appwrite/appwrite/pull/123" });
  });

  test("markSecurityLive posts the deployed version", async () => {
    const calls = mockFetch({ id: 9 });
    await markSecurityLive(9, "1.8.4");
    expect(calls[0].url).toBe("/api/security/vulnerabilities/9/live");
    expect(calls[0].method).toBe("POST");
    expect(calls[0].body).toEqual({ deployed_version: "1.8.4" });
  });

  test("a rejected action throws an ApiError carrying the status", async () => {
    mockFetch({}, 409);
    const error = await markSecurityLive(9, "1.8.4").catch((caught: unknown) => caught);
    expect(error).toBeInstanceOf(ApiError);
    expect((error as ApiError).status).toBe(409);
  });

  test("a failed summary read rejects", async () => {
    mockFetch({}, 403);
    expect(fetchSecuritySummary()).rejects.toThrow();
  });
});
