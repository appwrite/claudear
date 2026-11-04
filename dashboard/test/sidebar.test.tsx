import { describe, test, expect, afterEach, mock } from "bun:test";
import { render, screen, cleanup, fireEvent, waitFor } from "@testing-library/react";
import { Router, RouterProvider } from "../src/router";
import { Sidebar } from "../src/components/layout/sidebar";
import { AuthProvider } from "../src/lib/auth";
import { resetPrefetchedRoutes } from "../src/lib/route-prefetch";

function jsonResponse(data: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: "OK",
    json: () => Promise.resolve(data),
    text: () => Promise.resolve(JSON.stringify(data)),
    headers: new Headers({ "content-type": "application/json" }),
  };
}

describe("Sidebar", () => {
  const originalFetch = globalThis.fetch;

  afterEach(() => {
    cleanup();
    window.history.replaceState(null, "", "/");
    globalThis.fetch = originalFetch;
  });

  function renderSidebar(path = "/") {
    window.history.replaceState(null, "", path);
    return render(
      <RouterProvider>
        <Sidebar />
        <Router routes={{ [path]: () => <div /> }} />
      </RouterProvider>
    );
  }

  function renderSidebarWithAuth(
    path = "/",
    user = { id: 1, email: "admin@test.com", name: "Admin User", role: "admin" as string },
  ) {
    globalThis.fetch = mock((input: string | URL | Request) => {
      const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
      if (url.endsWith("/api/auth/me")) {
        return Promise.resolve(jsonResponse(user)) as Promise<Response>;
      }
      if (url.endsWith("/api/auth/logout")) {
        return Promise.resolve(jsonResponse(null, 204)) as Promise<Response>;
      }
      return Promise.resolve(jsonResponse({})) as Promise<Response>;
    }) as unknown as typeof fetch;

    window.history.replaceState(null, "", path);
    return render(
      <AuthProvider>
        <RouterProvider>
          <Sidebar />
          <Router routes={{ [path]: () => <div /> }} />
        </RouterProvider>
      </AuthProvider>
    );
  }

  test('renders "claudear" title', () => {
    renderSidebar();
    expect(screen.getByText("claudear")).toBeDefined();
  });

  test("renders all nav item labels", () => {
    renderSidebar();
    const labels = [
      "Overview",
      "Issues",
      "Attempts",
      "PRs",
      "Analytics",
      "Errors",
      "Feedback",
      "Regressions",
      "Experiments",
      "Repos",
      "Inference",
      "Activity",
      "Telemetry",
    ];
    for (const label of labels) {
      expect(screen.getByText(label)).toBeDefined();
    }
  });

  test("active item has correct styling on root path", () => {
    renderSidebar("/");
    const overviewButton = screen.getByText("Overview").closest("button");
    expect(overviewButton?.className).toContain("bg-primary");
    expect(overviewButton?.className).toContain("font-medium");
  });

  test("non-active items have muted styling", () => {
    renderSidebar("/");
    const attemptsButton = screen.getByText("Attempts").closest("button");
    expect(attemptsButton?.className).toContain("text-muted-foreground");
  });

  test("clicking a nav item updates the path", () => {
    renderSidebar("/");
    const attemptsButton = screen.getByText("Attempts").closest("button");
    fireEvent.click(attemptsButton!);
    expect(window.location.pathname).toBe("/attempts");
  });

  test("dark mode toggle adds dark class to documentElement", () => {
    document.documentElement.classList.remove("dark");
    localStorage.removeItem("theme");
    renderSidebar();
    const toggleBtn = screen.getByTitle("Switch to dark mode");
    fireEvent.click(toggleBtn);
    expect(document.documentElement.classList.contains("dark")).toBe(true);
    expect(localStorage.getItem("theme")).toBe("dark");
  });

  test("dark mode toggle removes dark class when already dark", () => {
    document.documentElement.classList.add("dark");
    localStorage.setItem("theme", "dark");
    renderSidebar();
    const toggleBtn = screen.getByTitle("Switch to light mode");
    fireEvent.click(toggleBtn);
    expect(document.documentElement.classList.contains("dark")).toBe(false);
    expect(localStorage.getItem("theme")).toBe("light");
  });

  test("renders Chat nav item", () => {
    renderSidebar();
    expect(screen.getByText("Chat")).toBeTruthy();
  });

  test("renders Learning nav item", () => {
    renderSidebar();
    expect(screen.getByText("Learning")).toBeTruthy();
  });

  test("shows admin section when user is admin", async () => {
    renderSidebarWithAuth("/");
    await waitFor(() => {
      expect(screen.getByText("Config")).toBeTruthy();
      expect(screen.getByText("Users")).toBeTruthy();
      expect(screen.getByText("Admin")).toBeTruthy();
    });
  });

  test("admin Config button navigates to /config", async () => {
    renderSidebarWithAuth("/");
    await waitFor(() => {
      expect(screen.getByText("Config")).toBeTruthy();
    });
    const configBtn = screen.getByText("Config").closest("button");
    fireEvent.click(configBtn!);
    expect(window.location.pathname).toBe("/config");
  });

  test("admin Users button navigates to /users", async () => {
    renderSidebarWithAuth("/");
    await waitFor(() => {
      expect(screen.getByText("Users")).toBeTruthy();
    });
    const usersBtn = screen.getByText("Users").closest("button");
    fireEvent.click(usersBtn!);
    expect(window.location.pathname).toBe("/users");
  });

  test("non-admin user sees Users but not the admin section", async () => {
    renderSidebarWithAuth("/", { id: 2, email: "user@test.com", name: "Regular", role: "viewer" });
    await waitFor(() => {
      expect(screen.getByText("Regular")).toBeTruthy();
    });
    expect(screen.getByText("Users")).toBeTruthy();
    expect(screen.queryByText("Config")).toBeNull();
    expect(screen.queryByText("Admin")).toBeNull();
  });

  test("non-admin Users button navigates to /users without prefetching the admin user list", async () => {
    renderSidebarWithAuth("/", { id: 2, email: "user@test.com", name: "Regular", role: "viewer" });
    await waitFor(() => {
      expect(screen.getByText("Users")).toBeTruthy();
    });
    resetPrefetchedRoutes();
    const usersBtn = screen.getByText("Users").closest("button");
    fireEvent.mouseEnter(usersBtn!);
    fireEvent.focus(usersBtn!);
    const requested = (globalThis.fetch as unknown as ReturnType<typeof mock>).mock.calls.map(([input]) => String(input));
    expect(requested.some((url) => url.endsWith("/api/users"))).toBe(false);
    fireEvent.click(usersBtn!);
    expect(window.location.pathname).toBe("/users");
  });

  test("signed-out sidebar shows neither Users nor Config", () => {
    renderSidebar();
    expect(screen.queryByText("Users")).toBeNull();
    expect(screen.queryByText("Config")).toBeNull();
  });

  test("shows user name and email when loaded", async () => {
    renderSidebarWithAuth("/", { id: 1, email: "jake@test.com", name: "Jake B", role: "admin" });
    await waitFor(() => {
      expect(screen.getByText("Jake B")).toBeTruthy();
      expect(screen.getByText("jake@test.com")).toBeTruthy();
    });
  });

  test("shows user initials when no avatar URL", async () => {
    renderSidebarWithAuth("/", { id: 1, email: "test@test.com", name: "John Doe", role: "viewer" });
    await waitFor(() => {
      expect(screen.getByText("JD")).toBeTruthy();
    });
  });

  test("sign out button is present", async () => {
    renderSidebarWithAuth("/");
    await waitFor(() => {
      expect(screen.getByTitle("Sign out")).toBeTruthy();
    });
  });

  test("documentation link is present", async () => {
    renderSidebarWithAuth("/");
    await waitFor(() => {
      expect(screen.getByTitle("Documentation")).toBeTruthy();
    });
  });

  test("account settings button is present", async () => {
    renderSidebarWithAuth("/");
    await waitFor(() => {
      expect(screen.getByTitle("Account settings")).toBeTruthy();
    });
  });

  test("hover on nav item triggers prefetch", () => {
    renderSidebar();
    const attemptsBtn = screen.getByText("Attempts").closest("button");
    fireEvent.mouseEnter(attemptsBtn!);
    // No error thrown = prefetch handler ran successfully
  });

  test("focus on nav item triggers prefetch", () => {
    renderSidebar();
    const attemptsBtn = screen.getByText("Attempts").closest("button");
    fireEvent.focus(attemptsBtn!);
  });
});
