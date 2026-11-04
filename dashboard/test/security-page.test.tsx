import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { SWRConfig } from "swr";
import type { ReactNode } from "react";
import { AuthProvider } from "../src/lib/auth";
import SecurityPage from "../src/pages/security";
import type {
  SecurityReportDetail,
  SecurityReportRow,
  SecuritySummary,
} from "../src/lib/api";

type Call = { url: string; method: string; body: unknown };

const admin = { id: 1, email: "admin@appwrite.io", name: "Admin", role: "admin", avatar_url: null };
const viewer = { id: 2, email: "viewer@appwrite.io", name: "Viewer", role: "viewer", avatar_url: null };

const summary: SecuritySummary = {
  reports_by_state: { received: 1, needs_human: 2, tracking: 3, closed: 4 },
  reports_by_outcome: { valid: 3 },
  vulnerabilities_by_state: { awaiting_fix: 1, fixing: 1, merged: 1, live: 5 },
  pending_awards: 7,
  unconfirmed_replies: 8,
};

function row(overrides: Partial<SecurityReportRow>): SecurityReportRow {
  return {
    id: 1,
    short_id: "HS-1001",
    url: "https://secure.helpscout.net/conversation/1001",
    subject: "Stored XSS in console",
    reporter_email: "first@researcher.test",
    reported_at: "2026-11-01T10:00:00Z",
    state: "needs_human",
    outcome: null,
    vulnerability_id: null,
    is_first_reporter: false,
    vulnerability_state: null,
    severity: null,
    pull_request_url: null,
    deployed_version: null,
    live_at: null,
    award_status: null,
    unconfirmed_reply: false,
    updated_at: "2026-11-01T10:05:00Z",
    ...overrides,
  };
}

const needsHumanRow = row({});
const trackedRow = row({
  id: 2,
  short_id: "HS-1002",
  subject: "Team member can read other team files",
  reporter_email: "second@researcher.test",
  state: "tracking",
  outcome: "valid",
  vulnerability_id: 7,
  is_first_reporter: true,
  vulnerability_state: "needs_human",
  severity: "high",
  pull_request_url: "https://github.com/appwrite/appwrite/pull/321",
  deployed_version: "1.8.4",
  award_status: "prepared",
  unconfirmed_reply: true,
});
const duplicateRow = row({
  id: 3,
  short_id: "HS-1003",
  reporter_email: "late@researcher.test",
  state: "closed",
  outcome: "duplicate",
  vulnerability_id: 7,
  vulnerability_state: "needs_human",
});

function report(source: SecurityReportRow) {
  return {
    id: source.id,
    source: "helpscout",
    conversation_id: String(1000 + source.id),
    short_id: source.short_id,
    mailbox_id: "249891",
    url: source.url,
    subject: source.subject,
    reporter_email: source.reporter_email,
    reporter_customer_id: null,
    reported_at: source.reported_at,
    state: source.state,
    outcome: source.outcome,
    public_reason: null,
    assessment_summary: source.id === 1 ? "Agent could not determine the impact." : null,
    affected_repository: source.id === 1 ? "appwrite/console" : null,
    fixed_by_commit: null,
    fixed_on: null,
    spam_confidence: 0.05,
    assessment_attempts: 3,
    customer_thread_count: 1,
    vulnerability_id: source.vulnerability_id,
    created_at: source.reported_at,
    updated_at: source.updated_at,
  };
}

const details: Record<number, SecurityReportDetail> = {
  1: {
    report: report(needsHumanRow),
    vulnerability: null,
    events: [
      {
        id: 11,
        report_id: 1,
        vulnerability_id: null,
        kind: "received",
        from_state: null,
        to_state: "received",
        actor: null,
        detail: {},
        created_at: "2026-11-01T10:00:00Z",
      },
      {
        id: 12,
        report_id: 1,
        vulnerability_id: null,
        kind: "needs_human",
        from_state: "assessing",
        to_state: "needs_human",
        actor: null,
        detail: { reason: "assessment failed three times" },
        created_at: "2026-11-01T10:05:00Z",
      },
    ],
    replies: [],
    award: null,
    related_reports: [],
  },
  2: {
    report: report(trackedRow),
    vulnerability: {
      id: 7,
      title: "Missing team permission check",
      summary: "Files endpoint does not check team membership.",
      repository: "appwrite/appwrite",
      severity: "high",
      state: "needs_human",
      first_report_id: 2,
      first_reported_at: "2026-11-01T10:00:00Z",
      fix_attempt_id: null,
      pull_request_url: "https://github.com/appwrite/appwrite/pull/321",
      merge_commit: null,
      merged_at: null,
      deployed_version: null,
      live_at: null,
      created_at: "2026-11-01T10:00:00Z",
      updated_at: "2026-11-01T10:00:00Z",
    },
    events: [
      {
        id: 21,
        report_id: null,
        vulnerability_id: 7,
        kind: "vulnerability_created",
        from_state: null,
        to_state: "awaiting_fix",
        actor: null,
        detail: null,
        created_at: "2026-11-01T11:00:00Z",
      },
      {
        id: 22,
        report_id: 2,
        vulnerability_id: null,
        kind: "reply_unconfirmed",
        from_state: null,
        to_state: null,
        actor: "admin@appwrite.io",
        detail: { kind: "confirmed" },
        created_at: "2026-11-01T11:05:00Z",
      },
    ],
    replies: [
      {
        id: 31,
        report_id: 2,
        kind: "confirmed",
        status: "unconfirmed",
        error: "timed out",
        claimed_at: "2026-11-01T11:04:00Z",
        completed_at: null,
      },
    ],
    award: {
      id: 41,
      vulnerability_id: 7,
      report_id: 2,
      recipient_email: "second@researcher.test",
      campaign: "SecurityResearchers",
      credits: 100,
      status: "prepared",
      actor: null,
      note: null,
      prepared_at: "2026-11-02T09:00:00Z",
      updated_at: "2026-11-02T09:00:00Z",
    },
    related_reports: [trackedRow, duplicateRow],
  },
  3: {
    report: report(duplicateRow),
    vulnerability: null,
    events: [],
    replies: [],
    award: null,
    related_reports: [],
  },
};

function harness(user: typeof admin, actionStatus = 200) {
  const calls: Call[] = [];
  globalThis.fetch = mock((input: string | URL | Request, init?: RequestInit) => {
    const url = String(input);
    const method = init?.method ?? "GET";
    calls.push({ url, method, body: typeof init?.body === "string" ? JSON.parse(init.body) : undefined });
    const respond = (data: unknown, status = 200) =>
      Promise.resolve({ ok: status >= 200 && status < 300, status, json: () => Promise.resolve(data) });

    if (url.endsWith("/api/auth/me")) return respond(user);
    if (method === "POST") return respond({}, actionStatus);
    if (url.endsWith("/api/security/summary")) return respond(summary);
    if (url.includes("/api/security/reports?")) return respond([needsHumanRow, trackedRow]);
    const detailMatch = url.match(/\/api\/security\/reports\/(\d+)$/);
    if (detailMatch) return respond(details[Number(detailMatch[1])]);
    return respond({}, 404);
  }) as unknown as typeof fetch;
  return calls;
}

function Wrapper({ children }: { children: ReactNode }) {
  return (
    <SWRConfig value={{ provider: () => new Map(), dedupingInterval: 0 }}>
      <AuthProvider>{children}</AuthProvider>
    </SWRConfig>
  );
}

function posts(calls: Call[]) {
  return calls.filter((call) => call.method === "POST");
}

async function openReport(shortId: string) {
  const cell = await screen.findByText(shortId);
  fireEvent.click(cell.closest("tr")!);
  return screen.findByText(`${shortId} security report`);
}

describe("SecurityPage", () => {
  const originalFetch = globalThis.fetch;
  const originalConfirm = window.confirm;

  beforeEach(() => {
    window.confirm = () => true;
  });

  afterEach(() => {
    cleanup();
    globalThis.fetch = originalFetch;
    window.confirm = originalConfirm;
  });

  test("non-admins see access denied and no security endpoint is called", async () => {
    const calls = harness(viewer);
    render(<SecurityPage />, { wrapper: Wrapper });
    await screen.findByText(/Access denied/);
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(calls.some((call) => call.url.includes("/api/security"))).toBe(false);
  });

  test("renders the summary cards from the summary endpoint", async () => {
    harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    const needsHuman = (await screen.findByText("Needs Human")).closest(".rounded-lg") as HTMLElement;
    expect(within(needsHuman).getByText("2")).toBeTruthy();
    const open = screen.getByText("Open Reports").closest(".rounded-lg") as HTMLElement;
    expect(within(open).getByText("6")).toBeTruthy();
    const inFlight = screen.getByText("Fixes In Flight").closest(".rounded-lg") as HTMLElement;
    expect(within(inFlight).getByText("3")).toBeTruthy();
    const live = screen.getByText("Live Fixes").closest(".rounded-lg") as HTMLElement;
    expect(within(live).getByText("5")).toBeTruthy();
    const awards = screen.getByText("Pending Awards").closest(".rounded-lg") as HTMLElement;
    expect(within(awards).getByText("7")).toBeTruthy();
    const unconfirmed = screen.getByText("Unconfirmed Replies").closest(".rounded-lg") as HTMLElement;
    expect(within(unconfirmed).getByText("8")).toBeTruthy();
  });

  test("renders a table row per report with its badges and links", async () => {
    harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    const tracked = (await screen.findByText("HS-1002")).closest("tr") as HTMLElement;
    expect(within(tracked).getByText("tracking")).toBeTruthy();
    expect(within(tracked).getByText("valid")).toBeTruthy();
    expect(within(tracked).getByText("First reporter")).toBeTruthy();
    expect(within(tracked).getByText("Unconfirmed reply")).toBeTruthy();
    expect(within(tracked).getByText("#7")).toBeTruthy();
    expect(within(tracked).getByText("high")).toBeTruthy();
    expect(within(tracked).getByText("1.8.4")).toBeTruthy();
    expect(within(tracked).getByText("prepared")).toBeTruthy();
    expect(within(tracked).getByText("View").closest("a")?.getAttribute("href")).toBe(
      "https://github.com/appwrite/appwrite/pull/321",
    );

    const pending = screen.getByText("HS-1001").closest("tr") as HTMLElement;
    expect(within(pending).getByText("needs human")).toBeTruthy();
    expect(within(pending).queryByText("First reporter")).toBeNull();
    expect(within(pending).queryByText("Unconfirmed reply")).toBeNull();
  });

  test("changing the state filter refetches with the filter", async () => {
    const calls = harness(admin);
    const { container } = render(<SecurityPage />, { wrapper: Wrapper });
    await screen.findByText("HS-1001");
    const [stateSelect] = Array.from(container.querySelectorAll("select"));
    fireEvent.change(stateSelect, { target: { value: "needs_human" } });
    await waitFor(() => {
      expect(calls.some((call) => call.url.includes("/api/security/reports?state=needs_human"))).toBe(true);
    });
  });

  test("the detail modal shows the full timeline and reply ledger", async () => {
    harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1002");
    const timeline = screen.getByRole("list", { name: "Security timeline" });
    expect(within(timeline).getByText("Vulnerability created")).toBeTruthy();
    expect(within(timeline).getByText("Reply unconfirmed")).toBeTruthy();
    expect(within(timeline).getByText("by admin@appwrite.io")).toBeTruthy();
    expect(screen.getByText("Vulnerability #7")).toBeTruthy();
    expect(screen.getByText("timed out")).toBeTruthy();
    expect(screen.getByText("Reports of the same vulnerability")).toBeTruthy();
    expect(screen.getByText("HS-1003")).toBeTruthy();
  });

  test("Reassess posts the reassess override", async () => {
    const calls = harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1001");
    fireEvent.click(screen.getByRole("button", { name: "Reassess" }));
    fireEvent.click(await screen.findByRole("button", { name: "Start reassessment" }));
    await waitFor(() => expect(posts(calls)).toHaveLength(1));
    expect(posts(calls)[0]).toEqual({
      url: "/api/security/reports/1/override",
      method: "POST",
      body: { action: "reassess" },
    });
  });

  test("Confirm valid posts the confirm_valid override with the form fields", async () => {
    const calls = harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1001");
    fireEvent.change(screen.getByLabelText("Title"), { target: { value: "Stored XSS in project name" } });
    fireEvent.change(screen.getByLabelText("Severity"), { target: { value: "medium" } });
    fireEvent.click(screen.getByRole("button", { name: "Confirm vulnerability" }));
    await waitFor(() => expect(posts(calls)).toHaveLength(1));
    expect(posts(calls)[0].url).toBe("/api/security/reports/1/override");
    expect(posts(calls)[0].body).toEqual({
      action: "confirm_valid",
      title: "Stored XSS in project name",
      summary: "Agent could not determine the impact.",
      repository: "appwrite/console",
      severity: "medium",
    });
  });

  test("Confirm valid rejects a malformed repository without posting", async () => {
    const calls = harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1001");
    fireEvent.change(screen.getByLabelText("Title"), { target: { value: "A title" } });
    fireEvent.change(screen.getByLabelText("Repository"), { target: { value: "not a repository" } });
    fireEvent.click(screen.getByRole("button", { name: "Confirm vulnerability" }));
    expect(await screen.findByText("The repository must look like owner/name.")).toBeTruthy();
    expect(posts(calls)).toHaveLength(0);
  });

  test("the outcome form requires a public reason and posts the outcome override", async () => {
    const calls = harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1001");
    fireEvent.click(screen.getByRole("button", { name: "Close with outcome" }));
    const outcome = screen.getByLabelText("Outcome") as HTMLSelectElement;
    expect(Array.from(outcome.options).map((option) => option.value)).not.toContain("valid");
    fireEvent.change(outcome, { target: { value: "out_of_scope" } });
    fireEvent.submit(screen.getByRole("button", { name: "Send outcome" }).closest("form")!);
    expect(await screen.findByText("A public reason is required for this outcome.")).toBeTruthy();
    expect(posts(calls)).toHaveLength(0);

    fireEvent.change(screen.getByLabelText("Public reason"), { target: { value: "  Self-XSS only.  " } });
    fireEvent.click(screen.getByRole("button", { name: "Send outcome" }));
    await waitFor(() => expect(posts(calls)).toHaveLength(1));
    expect(posts(calls)[0].body).toEqual({
      action: "outcome",
      outcome: "out_of_scope",
      public_reason: "Self-XSS only.",
    });
  });

  test("outcomes whose reply quotes no reason send no public reason", async () => {
    const calls = harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1001");
    fireEvent.click(screen.getByRole("button", { name: "Close with outcome" }));
    expect(screen.queryByLabelText("Public reason")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Send outcome" }));
    await waitFor(() => expect(posts(calls)).toHaveLength(1));
    expect(posts(calls)[0].body).toEqual({ action: "outcome", outcome: "not_reproducible" });
  });

  test("spam is sent as an outcome override", async () => {
    const calls = harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1001");
    fireEvent.click(screen.getByRole("button", { name: "Close with outcome" }));
    fireEvent.change(screen.getByLabelText("Outcome"), { target: { value: "spam" } });
    fireEvent.click(screen.getByRole("button", { name: "Mark as spam" }));
    await waitFor(() => expect(posts(calls)).toHaveLength(1));
    expect(posts(calls)[0].body).toEqual({ action: "outcome", outcome: "spam" });
  });

  test("already fixed sends fixed_on", async () => {
    const calls = harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1001");
    fireEvent.click(screen.getByRole("button", { name: "Close with outcome" }));
    fireEvent.change(screen.getByLabelText("Outcome"), { target: { value: "already_fixed" } });
    fireEvent.change(screen.getByLabelText("Fixed on"), { target: { value: "2026-10-20" } });
    fireEvent.click(screen.getByRole("button", { name: "Send outcome" }));
    await waitFor(() => expect(posts(calls)).toHaveLength(1));
    expect(posts(calls)[0].body).toEqual({ action: "outcome", outcome: "already_fixed", fixed_on: "2026-10-20" });
  });

  test("declining the confirmation prompt sends nothing", async () => {
    const calls = harness(admin);
    window.confirm = () => false;
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1001");
    fireEvent.change(screen.getByLabelText("Title"), { target: { value: "A title" } });
    fireEvent.click(screen.getByRole("button", { name: "Confirm vulnerability" }));
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(posts(calls)).toHaveLength(0);
  });

  test("delivery confirmation posts delivered true and false", async () => {
    const calls = harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1002");
    fireEvent.click(screen.getByRole("button", { name: "Delivered" }));
    await waitFor(() => expect(posts(calls)).toHaveLength(1));
    fireEvent.click(await screen.findByRole("button", { name: "Not delivered" }));
    await waitFor(() => expect(posts(calls)).toHaveLength(2));
    expect(posts(calls).map((call) => call.body)).toEqual([
      { action: "confirm_delivery", kind: "confirmed", delivered: true },
      { action: "confirm_delivery", kind: "confirmed", delivered: false },
    ]);
    expect(posts(calls).every((call) => call.url === "/api/security/reports/2/override")).toBe(true);
  });

  test("Not delivered asks first, warns that the reply is resent, and sends nothing when declined", async () => {
    const calls = harness(admin);
    const prompts: string[] = [];
    window.confirm = (message?: string) => {
      prompts.push(String(message));
      return false;
    };
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1002");
    fireEvent.click(screen.getByRole("button", { name: "Not delivered" }));
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(posts(calls)).toHaveLength(0);
    expect(prompts).toHaveLength(1);
    expect(prompts[0]).toContain("not delivered");
    expect(prompts[0]).toContain("send it to the reporter again");
  });

  test("Mark awarded asks first, names the reward and recipient, and sends nothing when declined", async () => {
    const calls = harness(admin);
    const prompts: string[] = [];
    window.confirm = (message?: string) => {
      prompts.push(String(message));
      return false;
    };
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1002");
    fireEvent.click(screen.getByRole("button", { name: "Mark awarded" }));
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(posts(calls)).toHaveLength(0);
    expect(prompts).toHaveLength(1);
    expect(prompts[0]).toContain("SecurityResearchers");
    expect(prompts[0]).toContain("second@researcher.test");
    expect(prompts[0]).toContain("cannot be undone");
  });

  test("Link PR posts the pull request url", async () => {
    const calls = harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1002");
    fireEvent.change(screen.getByLabelText("Pull request URL"), {
      target: { value: "https://github.com/appwrite/appwrite/pull/400" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Link PR" }));
    await waitFor(() => expect(posts(calls)).toHaveLength(1));
    expect(posts(calls)[0]).toEqual({
      url: "/api/security/vulnerabilities/7/pull-request",
      method: "POST",
      body: { pull_request_url: "https://github.com/appwrite/appwrite/pull/400" },
    });
  });

  test("Mark live posts the deployed version", async () => {
    const calls = harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1002");
    fireEvent.change(screen.getByLabelText("Deployed version"), { target: { value: "1.8.5" } });
    fireEvent.click(screen.getByRole("button", { name: "Mark live" }));
    await waitFor(() => expect(posts(calls)).toHaveLength(1));
    expect(posts(calls)[0]).toEqual({
      url: "/api/security/vulnerabilities/7/live",
      method: "POST",
      body: { deployed_version: "1.8.5" },
    });
  });

  test("Mark awarded posts the note", async () => {
    const calls = harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1002");
    expect(screen.getByText("SecurityResearchers")).toBeTruthy();
    fireEvent.change(screen.getByLabelText("Note (optional)"), { target: { value: "Sent by email" } });
    fireEvent.click(screen.getByRole("button", { name: "Mark awarded" }));
    await waitFor(() => expect(posts(calls)).toHaveLength(1));
    expect(posts(calls)[0]).toEqual({
      url: "/api/security/vulnerabilities/7/award",
      method: "POST",
      body: { note: "Sent by email" },
    });
  });

  test("a conflict shows the changed-state message", async () => {
    harness(admin, 409);
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1002");
    fireEvent.change(screen.getByLabelText("Deployed version"), { target: { value: "1.8.5" } });
    fireEvent.click(screen.getByRole("button", { name: "Mark live" }));
    expect(await screen.findByText(/changed state since it was loaded/)).toBeTruthy();
  });

  test("selecting a related report opens it", async () => {
    harness(admin);
    render(<SecurityPage />, { wrapper: Wrapper });
    await openReport("HS-1002");
    fireEvent.click(screen.getByText("HS-1003").closest("button")!);
    expect(await screen.findByText("HS-1003 security report")).toBeTruthy();
  });
});
