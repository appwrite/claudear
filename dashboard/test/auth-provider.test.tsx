import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import useSWR, { SWRConfig } from "swr";
import { getMe } from "../src/lib/api";
import { AuthProvider, useAuth } from "../src/lib/auth";

function jsonResponse(data: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: status >= 200 && status < 300 ? "OK" : "Error",
    json: () => Promise.resolve(data),
    text: () => Promise.resolve(JSON.stringify(data)),
    headers: new Headers({ "content-type": "application/json" }),
  };
}

function TestHarness() {
  const auth = useAuth();

  return (
    <div>
      <div data-testid="loading">{String(auth.loading)}</div>
      <div data-testid="user">{auth.user ? `${auth.user.email}|${auth.user.name}` : "none"}</div>
      <button onClick={() => void auth.login("login@test.com", "secret")}>login</button>
      <button onClick={() => void auth.logout()}>logout</button>
      <button onClick={() => void auth.refreshUser().catch(() => {})}>refresh</button>
      <button
        onClick={() =>
          void auth.updateProfile({ name: "Renamed", current_password: "old-password" }).catch(() => {})
        }
      >
        update-profile
      </button>
    </div>
  );
}

describe("AuthProvider", () => {
  const originalFetch = globalThis.fetch;
  beforeEach(() => {});

  afterEach(() => {
    cleanup();
    globalThis.fetch = originalFetch;
  });

  test("loads current user and supports login, profile update, refresh, and logout", async () => {
    const meResponses = [
      { id: 1, email: "initial@test.com", name: "Initial", role: "admin" },
      { id: 2, email: "updated@test.com", name: "Updated", role: "admin" },
      { id: 3, email: "refreshed@test.com", name: "Refreshed", role: "admin" },
    ];
    let getMeIndex = 0;
    const fetchCalls: Array<{ url: string; method: string }> = [];

    globalThis.fetch = mock((input: string | URL | Request, init?: RequestInit) => {
      const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
      const method = init?.method ?? (input instanceof Request ? input.method : "GET");
      fetchCalls.push({ url, method });

      if (url.endsWith("/api/auth/me")) {
        const next = meResponses[Math.min(getMeIndex, meResponses.length - 1)];
        getMeIndex += 1;
        return Promise.resolve(jsonResponse(next)) as Promise<Response>;
      }
      if (url.endsWith("/api/auth/login") && method === "POST") {
        return Promise.resolve(
          jsonResponse({
            user: { id: 9, email: "login@test.com", name: "Logged In", role: "admin" },
          })
        ) as Promise<Response>;
      }
      if (url.endsWith("/api/auth/profile") && method === "PUT") {
        return Promise.resolve(
          jsonResponse({ id: 2, email: "updated@test.com", name: "Updated", role: "admin" })
        ) as Promise<Response>;
      }
      if (url.endsWith("/api/auth/logout") && method === "POST") {
        return Promise.resolve(jsonResponse(null, 204)) as Promise<Response>;
      }

      return Promise.resolve(jsonResponse({})) as Promise<Response>;
    }) as unknown as typeof fetch;

    render(
      <AuthProvider>
        <TestHarness />
      </AuthProvider>
    );

    await waitFor(() => {
      expect(screen.getByTestId("loading").textContent).toBe("false");
      expect(screen.getByTestId("user").textContent).toBe("initial@test.com|Initial");
    });

    fireEvent.click(screen.getByText("login"));
    await waitFor(() => {
      expect(screen.getByTestId("user").textContent).toBe("login@test.com|Logged In");
    });

    fireEvent.click(screen.getByText("update-profile"));
    await waitFor(() => {
      expect(screen.getByTestId("user").textContent).toBe("updated@test.com|Updated");
    });

    fireEvent.click(screen.getByText("refresh"));
    await waitFor(() => {
      expect(screen.getByTestId("user").textContent).toBe("refreshed@test.com|Refreshed");
    });

    fireEvent.click(screen.getByText("logout"));
    await waitFor(() => {
      expect(screen.getByTestId("user").textContent).toBe("none");
    });

    expect(fetchCalls.some((call) => call.url.endsWith("/api/auth/login") && call.method === "POST")).toBe(true);
    expect(fetchCalls.some((call) => call.url.endsWith("/api/auth/profile") && call.method === "PUT")).toBe(true);
    expect(fetchCalls.some((call) => call.url.endsWith("/api/auth/logout") && call.method === "POST")).toBe(true);
    expect(fetchCalls.filter((call) => call.url.endsWith("/api/auth/me")).length).toBe(3);

  });

  test("logging out drops every cached response so the next user never sees them", async () => {
    globalThis.fetch = mock((input: string | URL | Request) => {
      const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
      if (url.endsWith("/api/auth/me")) {
        return Promise.resolve(
          jsonResponse({ id: 1, email: "admin@test.com", name: "Admin", role: "admin" })
        ) as Promise<Response>;
      }
      return Promise.resolve(jsonResponse(null, 204)) as Promise<Response>;
    }) as unknown as typeof fetch;
    const cache = new Map();

    function CachedSecrets() {
      const { data } = useSWR("/api/security/summary", () => Promise.resolve("admin-only summary"));
      return <div data-testid="cached">{data ?? "empty"}</div>;
    }

    render(
      <SWRConfig value={{ provider: () => cache, dedupingInterval: 0 }}>
        <AuthProvider>
          <TestHarness />
          <CachedSecrets />
        </AuthProvider>
      </SWRConfig>
    );

    await waitFor(() => {
      expect(screen.getByTestId("user").textContent).toBe("admin@test.com|Admin");
      expect(screen.getByTestId("cached").textContent).toBe("admin-only summary");
    });

    fireEvent.click(screen.getByText("logout"));

    await waitFor(() => {
      expect(screen.getByTestId("user").textContent).toBe("none");
      expect(screen.getByTestId("cached").textContent).toBe("empty");
    });
    expect(cache.get("/api/security/summary")?.data).toBeUndefined();
  });

  test("clears user when initial auth check fails", async () => {
    globalThis.fetch = mock(() =>
      Promise.resolve(jsonResponse({ error: "unauthorized" }, 401))
    ) as unknown as typeof fetch;

    render(
      <AuthProvider>
        <TestHarness />
      </AuthProvider>
    );

    await waitFor(() => {
      expect(screen.getByTestId("loading").textContent).toBe("false");
      expect(screen.getByTestId("user").textContent).toBe("none");
    });
  });

  test("uses onUnauthorized callback to clear the user during a later request", async () => {
    let meCalls = 0;
    globalThis.fetch = mock((input: string | URL | Request) => {
      const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
      if (url.endsWith("/api/auth/me")) {
        meCalls += 1;
        if (meCalls === 1) {
          return Promise.resolve(
            jsonResponse({ id: 1, email: "user@test.com", name: "User", role: "admin" })
          ) as Promise<Response>;
        }
        return Promise.resolve(jsonResponse({ error: "unauthorized" }, 401)) as Promise<Response>;
      }

      return Promise.resolve(jsonResponse({})) as Promise<Response>;
    }) as unknown as typeof fetch;

    render(
      <AuthProvider>
        <TestHarness />
      </AuthProvider>
    );

    await waitFor(() => {
      expect(screen.getByTestId("user").textContent).toBe("user@test.com|User");
    });

    fireEvent.click(screen.getByText("refresh"));

    await waitFor(() => {
      expect(screen.getByTestId("user").textContent).toBe("none");
    });
  });

  test("an unmounted provider no longer handles a 401", async () => {
    let unauthorized = false;
    globalThis.fetch = mock((input: string | URL | Request) => {
      const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
      if (unauthorized || !url.endsWith("/api/auth/me")) {
        return Promise.resolve(jsonResponse({ error: "unauthorized" }, 401)) as Promise<Response>;
      }
      return Promise.resolve(
        jsonResponse({ id: 1, email: "user@test.com", name: "User", role: "admin" })
      ) as Promise<Response>;
    }) as unknown as typeof fetch;
    const cache = new Map();

    render(
      <SWRConfig value={{ provider: () => cache }}>
        <AuthProvider>
          <TestHarness />
        </AuthProvider>
      </SWRConfig>
    );
    await waitFor(() => {
      expect(screen.getByTestId("user").textContent).toBe("user@test.com|User");
    });
    cleanup();
    cache.set("/api/kept", { data: "kept" });
    unauthorized = true;

    await getMe().catch(() => {});
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(cache.get("/api/kept")?.data).toBe("kept");
  });
});
