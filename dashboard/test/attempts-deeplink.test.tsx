import { afterEach, describe, expect, mock, test } from "bun:test";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SWRConfig } from "swr";
import type { ReactNode } from "react";

const originalFetch = globalThis.fetch;

const attemptsList = {
  attempts: [
    {
      id: 3,
      source: "github_issues",
      short_id: "GH-3",
      title: "List row",
      status: "failed",
      pr_url: null,
      attempted_at: "2026-01-01T00:00:00Z",
      retry_count: 0,
    },
  ],
  total: 1,
  page: 1,
  per_page: 20,
};

const attemptSeven = {
  attempt: {
    id: 7,
    issue_id: "tip-7",
    short_id: "QA-7",
    source: "deploy_qa",
    attempted_at: "2026-01-01T00:00:00Z",
    pr_url: null,
    scm_repo: "appwrite/claudear",
    scm_pr_number: null,
    status: "pending",
    error_message: null,
    merged_at: null,
    resolved_at: null,
    retry_count: 0,
    last_retry_at: null,
    issue_labels: [],
    parent_attempt_id: null,
    cascade_repo: null,
  },
  executions: [],
  reviews: [],
  feedback: null,
};

function Wrapper({ children }: { children: ReactNode }) {
  return (
    <SWRConfig value={{ provider: () => new Map(), dedupingInterval: 0 }}>
      {children}
    </SWRConfig>
  );
}

function installFetch() {
  const urls: string[] = [];
  globalThis.fetch = mock((input: string | URL | Request) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
    urls.push(url);
    const body = url.startsWith("/api/attempts/7/detail")
      ? attemptSeven
      : url.startsWith("/api/attempts?")
        ? attemptsList
        : [];
    return Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve(body) });
  }) as unknown as typeof fetch;
  return urls;
}

async function renderAttempts() {
  const AttemptsPage = (await import("../src/pages/attempts")).default;
  render(<AttemptsPage />, { wrapper: Wrapper });
}

describe("AttemptsPage deep link", () => {
  afterEach(() => {
    cleanup();
    globalThis.fetch = originalFetch;
    window.history.replaceState(null, "", "/");
  });

  test("opens the detail for ?id=7 on mount", async () => {
    window.history.replaceState(null, "", "/attempts?id=7");
    const urls = installFetch();

    await renderAttempts();

    await waitFor(() => {
      expect(screen.getByText("Attempt Detail: QA-7")).toBeTruthy();
    });
    expect(urls).toContain("/api/attempts/7/detail");
    expect(screen.getByText("Deploy QA - tip-7")).toBeTruthy();
  });

  test("ignores a non-numeric id", async () => {
    window.history.replaceState(null, "", "/attempts?id=7abc");
    const urls = installFetch();

    await renderAttempts();

    await waitFor(() => {
      expect(screen.getByText("GH-3")).toBeTruthy();
    });
    expect(urls.some((url) => url.includes("/detail"))).toBe(false);
    expect(screen.queryByText(/Attempt Detail/)).toBeNull();
  });

  test("lists the real sources and filters by the stored name", async () => {
    window.history.replaceState(null, "", "/attempts");
    const urls = installFetch();

    await renderAttempts();

    await waitFor(() => {
      expect(screen.getByText("GH-3")).toBeTruthy();
    });
    expect(screen.getByText("GitHub Issues", { selector: "td span" })).toBeTruthy();

    const sourceSelect = screen.getByDisplayValue("All sources") as HTMLSelectElement;
    const values = Array.from(sourceSelect.options).map((option) => option.value);
    expect(values).toEqual([
      "",
      "linear",
      "sentry",
      "jira",
      "gitlab",
      "github_issues",
      "github_code_scanning",
      "discord",
      "slack",
      "helpscout",
      "whatsapp",
      "telegram",
      "deploy_qa",
    ]);

    fireEvent.change(sourceSelect, { target: { value: "github_code_scanning" } });

    await waitFor(() => {
      expect(urls.some((url) => url.includes("source=github_code_scanning"))).toBe(true);
    });
  });
});
