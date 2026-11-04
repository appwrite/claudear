import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { SWRConfig } from "swr";
import type { ReactNode } from "react";
import { AuthProvider } from "../src/lib/auth";

type TokenRecord = {
  id: string;
  user_id: number;
  name: string;
  token_prefix: string;
  created_at: string;
  last_used_at: string | null;
  expires_at: string | null;
};

type Call = { url: string; method: string; body?: Record<string, unknown> };

const originalFetch = globalThis.fetch;
const originalConfirm = globalThis.confirm;

const secret = "cldr_0123456789abcdef0123456789abcdef0123456789abcdef";

function Wrapper({ children }: { children: ReactNode }) {
  return (
    <SWRConfig value={{ provider: () => new Map(), dedupingInterval: 0 }}>
      <AuthProvider>{children}</AuthProvider>
    </SWRConfig>
  );
}

function respond(status: number, body: unknown) {
  return Promise.resolve({
    ok: status >= 200 && status < 300,
    status,
    json: () => Promise.resolve(body),
  });
}

function createTokensHarness(initial: TokenRecord[]) {
  let tokens = [...initial];
  const calls: Call[] = [];
  let nextCreateError: { status: number; body: unknown } | null = null;

  globalThis.fetch = mock((input: string | URL | Request, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
    const method = init?.method ?? "GET";
    const body = typeof init?.body === "string" ? JSON.parse(init.body) : undefined;
    calls.push({ url, method, body });

    if (url.endsWith("/api/auth/me")) {
      return respond(200, { id: 2, email: "viewer@test.com", name: "Viewer", role: "viewer", avatar_url: null });
    }

    if (url.endsWith("/api/tokens") && method === "GET") {
      return respond(200, tokens);
    }

    if (url.endsWith("/api/tokens") && method === "POST") {
      if (nextCreateError) {
        const failure = nextCreateError;
        nextCreateError = null;
        return respond(failure.status, failure.body);
      }
      const created: TokenRecord = {
        id: `token-${tokens.length + 1}`,
        user_id: 2,
        name: body.name,
        token_prefix: secret.slice(0, 13),
        created_at: "2026-01-02 00:00:00",
        last_used_at: null,
        expires_at: body.expires_at ?? null,
      };
      tokens = [...tokens, created];
      return respond(201, { ...created, secret });
    }

    if (url.includes("/api/tokens/") && method === "DELETE") {
      const id = url.split("/").pop();
      tokens = tokens.filter((token) => token.id !== id);
      return respond(204, null);
    }

    return respond(200, {});
  }) as unknown as typeof fetch;

  return {
    calls,
    failNextCreate(status: number, body: unknown) {
      nextCreateError = { status, body };
    },
  };
}

const existingToken: TokenRecord = {
  id: "token-existing",
  user_id: 2,
  name: "laptop",
  token_prefix: "cldr_aaaabbbb",
  created_at: "2026-01-01 00:00:00",
  last_used_at: null,
  expires_at: null,
};

async function renderSettings() {
  const SettingsPage = (await import("../src/pages/settings")).default;
  render(<SettingsPage />, { wrapper: Wrapper });
}

describe("Settings personal access tokens", () => {
  beforeEach(() => {
    globalThis.confirm = () => true;
  });

  afterEach(() => {
    cleanup();
    globalThis.fetch = originalFetch;
    globalThis.confirm = originalConfirm;
  });

  test("lists the current user's tokens and explains the MCP endpoint", async () => {
    createTokensHarness([existingToken]);

    await renderSettings();

    await waitFor(() => {
      expect(screen.getByText("laptop")).toBeTruthy();
    });
    expect(screen.getByText("Personal access tokens")).toBeTruthy();
    expect(screen.getByText("/mcp")).toBeTruthy();
    expect(screen.getByText("cldr_aaaabbbb…")).toBeTruthy();
    expect(screen.queryByText(secret)).toBeNull();
  });

  test("creates a token and shows its secret only once", async () => {
    const harness = createTokensHarness([]);

    await renderSettings();

    await waitFor(() => {
      expect(screen.getByText("No tokens yet.")).toBeTruthy();
    });

    fireEvent.change(screen.getByLabelText("Token name"), { target: { value: "  ci  " } });
    fireEvent.click(screen.getByRole("button", { name: "Create token" }));

    await waitFor(() => {
      expect(screen.getByText(secret)).toBeTruthy();
    });
    expect(screen.getByText(/won't be shown again/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Copy" })).toBeTruthy();

    const create = harness.calls.find((call) => call.method === "POST");
    expect(create?.url).toBe("/api/tokens");
    expect(create?.body).toEqual({ name: "ci" });

    await waitFor(() => {
      expect(screen.getByText("ci")).toBeTruthy();
    });

    fireEvent.click(screen.getByRole("button", { name: "Done" }));

    expect(screen.queryByText(secret)).toBeNull();
    expect(screen.getByText("ci")).toBeTruthy();
  });

  test("sends an RFC3339 expiry when a lifetime is chosen", async () => {
    const harness = createTokensHarness([]);

    await renderSettings();

    await waitFor(() => {
      expect(screen.getByText("No tokens yet.")).toBeTruthy();
    });

    fireEvent.change(screen.getByLabelText("Token name"), { target: { value: "short-lived" } });
    fireEvent.change(screen.getByDisplayValue("No expiry"), { target: { value: "30" } });
    const before = Date.now();
    fireEvent.click(screen.getByRole("button", { name: "Create token" }));

    await waitFor(() => {
      expect(harness.calls.some((call) => call.method === "POST")).toBe(true);
    });
    const create = harness.calls.find((call) => call.method === "POST");
    const expiresAt = create?.body?.expires_at;
    expect(typeof expiresAt).toBe("string");
    const days = (Date.parse(expiresAt as string) - before) / (24 * 60 * 60 * 1000);
    expect(days).toBeGreaterThan(29.9);
    expect(days).toBeLessThan(30.1);
  });

  test("revokes a token", async () => {
    const harness = createTokensHarness([existingToken]);

    await renderSettings();

    await waitFor(() => {
      expect(screen.getByText("laptop")).toBeTruthy();
    });

    fireEvent.click(screen.getByRole("button", { name: "Revoke laptop" }));

    await waitFor(() => {
      expect(screen.getByText("No tokens yet.")).toBeTruthy();
    });
    const revoke = harness.calls.find((call) => call.method === "DELETE");
    expect(revoke?.url).toBe("/api/tokens/token-existing");
  });

  test("keeps a token when revocation is not confirmed", async () => {
    const harness = createTokensHarness([existingToken]);
    globalThis.confirm = () => false;

    await renderSettings();

    await waitFor(() => {
      expect(screen.getByText("laptop")).toBeTruthy();
    });

    fireEvent.click(screen.getByRole("button", { name: "Revoke laptop" }));

    expect(harness.calls.some((call) => call.method === "DELETE")).toBe(false);
    expect(screen.getByText("laptop")).toBeTruthy();
  });

  test("shows the server error message when creation fails", async () => {
    const harness = createTokensHarness([]);
    harness.failNextCreate(429, { error: "Too many requests, slow down" });

    await renderSettings();

    await waitFor(() => {
      expect(screen.getByText("No tokens yet.")).toBeTruthy();
    });

    fireEvent.change(screen.getByLabelText("Token name"), { target: { value: "ci" } });
    fireEvent.click(screen.getByRole("button", { name: "Create token" }));

    await waitFor(() => {
      expect(within(screen.getByRole("alert")).getByText("Too many requests, slow down")).toBeTruthy();
    });
    expect(screen.queryByText(secret)).toBeNull();
  });
});
