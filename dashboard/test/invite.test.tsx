import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SWRConfig } from "swr";
import type { ReactNode } from "react";
import App from "../src/App";
import { AuthProvider } from "../src/lib/auth";
import InvitePage, { passwordProblem } from "../src/pages/invite";
import UsersPage, { describeInviteExpiry, hasOpenInvite } from "../src/pages/users";
import { isInvitePath } from "../src/lib/invite-token";
import type { Identities, UserIdentities, UserRecord } from "../src/lib/api";

type Call = { url: string; method: string; body?: unknown };
type Reply = { status: number; body?: unknown };
type Route = (call: Call) => Reply | undefined;

const DAY_MS = 24 * 60 * 60 * 1000;
const TOKEN = "cldi_0123456789abcdef";
const INVALID = "This invite link is invalid or has expired";
const LENGTH_MESSAGE = "Password must be 8-72 bytes";

const admin = { id: 1, email: "admin@test.com", name: "Admin", role: "admin" };

const emptyIdentities: Identities = {
  linear_names: [],
  github_usernames: [],
  sentry_usernames: [],
  jira_usernames: [],
  gitlab_usernames: [],
  discord_id: null,
  slack_id: null,
  email: null,
  push_user_key: null,
  sms_number: null,
  whatsapp_number: null,
  telegram_chat_id: null,
};

function sqliteTimestamp(offsetMs: number): string {
  return new Date(Date.now() + offsetMs).toISOString().slice(0, 19).replace("T", " ");
}

function userRecord(overrides: Partial<UserRecord> = {}): UserRecord {
  return {
    id: 7,
    email: "amy@example.com",
    name: "Amy",
    role: "viewer",
    avatar_url: null,
    status: "active",
    invite_expires_at: null,
    created_at: "2026-11-01 10:00:00",
    updated_at: "2026-11-01 10:00:00",
    ...overrides,
  };
}

function userIdentities(userId: number, name: string): UserIdentities {
  return {
    user_id: userId,
    name,
    login_email: null,
    person_key: null,
    slug: null,
    origin: "unset",
    status: "invited",
    redacted: false,
    identities: emptyIdentities,
    updated_at: null,
  };
}

function installFetch(routes: Route[]) {
  const calls: Call[] = [];
  globalThis.fetch = mock((input: string | URL | Request, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
    const method = init?.method ?? "GET";
    const body = typeof init?.body === "string" ? JSON.parse(init.body) : undefined;
    const call = { url, method, body };
    calls.push(call);
    const reply = routes.map((route) => route(call)).find((candidate) => candidate !== undefined) ?? {
      status: 404,
      body: { error: "not found" },
    };
    return Promise.resolve({
      ok: reply.status >= 200 && reply.status < 300,
      status: reply.status,
      json: () => Promise.resolve(reply.body),
    });
  }) as unknown as typeof fetch;
  return calls;
}

function route(method: string, path: string, reply: Reply | ((call: Call) => Reply)): Route {
  return (call) => {
    if (call.method !== method || new URL(call.url, "http://localhost").pathname !== path) return undefined;
    return typeof reply === "function" ? reply(call) : reply;
  };
}

function paths(calls: Call[]): string[] {
  return calls.map((call) => new URL(call.url, "http://localhost").pathname);
}

function inspectRoute(reply: Reply): Route {
  return route("POST", "/api/auth/invite/inspect", reply);
}

const validInvite: Reply = {
  status: 200,
  body: { name: "Amy", email: "amy@example.com", expires_at: sqliteTimestamp(6 * DAY_MS) },
};

function Wrapper({ children }: { children: ReactNode }) {
  return (
    <SWRConfig value={{ provider: () => new Map(), dedupingInterval: 0 }}>
      <AuthProvider>{children}</AuthProvider>
    </SWRConfig>
  );
}

function stubClipboard(writeText: (text: string) => Promise<void>) {
  const spy = mock(writeText);
  Object.defineProperty(navigator, "clipboard", { value: { writeText: spy }, configurable: true });
  return spy;
}

function visit(url: string) {
  window.history.replaceState(null, "", url);
}

describe("InvitePage", () => {
  const originalFetch = globalThis.fetch;

  afterEach(() => {
    cleanup();
    globalThis.fetch = originalFetch;
    visit("/");
  });

  test("strips the secret from the URL before inspecting it", async () => {
    const calls = installFetch([inspectRoute(validInvite)]);
    visit(`/invite#${TOKEN}`);
    expect(window.location.hash).toBe(`#${TOKEN}`);

    render(<InvitePage />);

    expect(window.location.hash).toBe("");
    expect(window.location.href).toBe("http://localhost/invite");
    await waitFor(() => expect(screen.getByRole("heading", { name: "Welcome, Amy" })).toBeTruthy());
    expect(screen.getByText(/amy@example\.com/)).toBeTruthy();
    const inspect = calls.find((call) => call.url.endsWith("/api/auth/invite/inspect"));
    expect(inspect?.method).toBe("POST");
    expect(inspect?.body).toEqual({ token: TOKEN });
  });

  test("keeps the token in history state so a reload still works", async () => {
    installFetch([inspectRoute(validInvite)]);
    visit(`/invite#${TOKEN}`);
    render(<InvitePage />);
    await waitFor(() => expect(screen.getByRole("heading", { name: "Welcome, Amy" })).toBeTruthy());
    cleanup();

    const calls = installFetch([inspectRoute(validInvite)]);
    render(<InvitePage />);
    await waitFor(() => expect(screen.getByRole("heading", { name: "Welcome, Amy" })).toBeTruthy());
    expect(calls[0]?.body).toEqual({ token: TOKEN });
  });

  test("accepts the invite with the token and chosen password, then offers sign in", async () => {
    const calls = installFetch([
      inspectRoute(validInvite),
      route("POST", "/api/auth/invite/accept", { status: 200, body: { message: "Invite accepted" } }),
    ]);
    visit(`/invite#${TOKEN}`);
    render(<InvitePage />);
    await waitFor(() => expect(screen.getByLabelText("Password")).toBeTruthy());

    fireEvent.change(screen.getByLabelText("Password"), { target: { value: "correct horse" } });
    fireEvent.change(screen.getByLabelText("Confirm password"), { target: { value: "correct horse" } });
    fireEvent.click(screen.getByRole("button", { name: "Set password" }));

    await waitFor(() => expect(screen.getByRole("heading", { name: "You're all set" })).toBeTruthy());
    const accept = calls.find((call) => call.url.endsWith("/api/auth/invite/accept"));
    expect(accept?.method).toBe("POST");
    expect(accept?.body).toEqual({ token: TOKEN, password: "correct horse" });
    expect(screen.getByRole("link", { name: "Sign in" }).getAttribute("href")).toBe("/");
    expect(window.history.state).toBeNull();
    expect(window.location.pathname).toBe("/invite");
  });

  test("a reload after accepting no longer carries the used token", async () => {
    installFetch([
      inspectRoute(validInvite),
      route("POST", "/api/auth/invite/accept", { status: 200, body: { message: "Invite accepted" } }),
    ]);
    visit(`/invite#${TOKEN}`);
    render(<InvitePage />);
    await waitFor(() => expect(screen.getByLabelText("Password")).toBeTruthy());
    fireEvent.change(screen.getByLabelText("Password"), { target: { value: "correct horse" } });
    fireEvent.change(screen.getByLabelText("Confirm password"), { target: { value: "correct horse" } });
    fireEvent.click(screen.getByRole("button", { name: "Set password" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "You're all set" })).toBeTruthy());
    cleanup();

    const calls = installFetch([inspectRoute(validInvite)]);
    render(<InvitePage />);

    expect(screen.getByRole("alert").textContent).toContain(INVALID);
    expect(calls).toHaveLength(0);
  });

  test("rejects mismatched or short passwords without calling the server", async () => {
    const calls = installFetch([inspectRoute(validInvite)]);
    visit(`/invite#${TOKEN}`);
    render(<InvitePage />);
    await waitFor(() => expect(screen.getByLabelText("Password")).toBeTruthy());

    fireEvent.change(screen.getByLabelText("Password"), { target: { value: "correct horse" } });
    fireEvent.change(screen.getByLabelText("Confirm password"), { target: { value: "correct house" } });
    fireEvent.submit(screen.getByRole("button", { name: "Set password" }));

    expect(screen.getByRole("alert").textContent).toBe("Passwords do not match");
    expect(paths(calls)).not.toContain("/api/auth/invite/accept");
    expect(passwordProblem("short", "short")).toBe(LENGTH_MESSAGE);
    expect(passwordProblem("x".repeat(73), "x".repeat(73))).toBe(LENGTH_MESSAGE);
    expect(passwordProblem("x".repeat(72), "x".repeat(72))).toBeNull();
  });

  test("counts password length in UTF-8 bytes like the server", async () => {
    const emoji = "\u{1F600}";
    expect(passwordProblem(emoji.repeat(18), emoji.repeat(18))).toBeNull();
    expect(passwordProblem(emoji.repeat(19), emoji.repeat(19))).toBe(LENGTH_MESSAGE);
    expect(passwordProblem("\u00e9".repeat(4), "\u00e9".repeat(4))).toBeNull();

    const calls = installFetch([inspectRoute(validInvite)]);
    visit(`/invite#${TOKEN}`);
    render(<InvitePage />);
    await waitFor(() => expect(screen.getByLabelText("Password")).toBeTruthy());

    const tooLong = emoji.repeat(30);
    expect(tooLong.length).toBeLessThanOrEqual(72);
    fireEvent.change(screen.getByLabelText("Password"), { target: { value: tooLong } });
    fireEvent.change(screen.getByLabelText("Confirm password"), { target: { value: tooLong } });
    fireEvent.submit(screen.getByRole("button", { name: "Set password" }));

    expect(screen.getByRole("alert").textContent).toBe(LENGTH_MESSAGE);
    expect(paths(calls)).not.toContain("/api/auth/invite/accept");
  });

  test("shows the error card for an invalid or expired token", async () => {
    const calls = installFetch([
      inspectRoute({ status: 404, body: { error: "This invite link is invalid or has expired" } }),
    ]);
    visit("/invite#cldi_expired");
    render(<InvitePage />);

    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain(INVALID));
    expect(screen.queryByLabelText("Password")).toBeNull();
    expect(paths(calls)).toEqual(["/api/auth/invite/inspect"]);
  });

  test("shows the error card when the link has no token, without calling the server", async () => {
    const calls = installFetch([]);
    visit("/invite");
    render(<InvitePage />);

    expect(screen.getByRole("alert").textContent).toContain(INVALID);
    expect(calls).toHaveLength(0);
  });

  test("switches to the error card when the invite is used up before accepting", async () => {
    installFetch([
      inspectRoute(validInvite),
      route("POST", "/api/auth/invite/accept", { status: 404, body: { error: INVALID } }),
    ]);
    visit(`/invite#${TOKEN}`);
    render(<InvitePage />);
    await waitFor(() => expect(screen.getByLabelText("Password")).toBeTruthy());

    fireEvent.change(screen.getByLabelText("Password"), { target: { value: "correct horse" } });
    fireEvent.change(screen.getByLabelText("Confirm password"), { target: { value: "correct horse" } });
    fireEvent.click(screen.getByRole("button", { name: "Set password" }));

    await waitFor(() => expect(screen.getByRole("heading", { name: "Invite unavailable" })).toBeTruthy());
  });

  test("reloads when a new invite link is opened in the same tab", async () => {
    installFetch([inspectRoute(validInvite)]);
    visit(`/invite#${TOKEN}`);
    render(<InvitePage />);
    await waitFor(() => expect(screen.getByRole("heading", { name: "Welcome, Amy" })).toBeTruthy());
    const reload = mock(() => {});
    const original = Object.getOwnPropertyDescriptor(window.location, "reload");
    Object.defineProperty(window.location, "reload", { value: reload, configurable: true });
    try {
      visit("/invite#cldi_another");
      window.dispatchEvent(new Event("hashchange"));
      expect(reload).toHaveBeenCalledTimes(1);
    } finally {
      if (original) Object.defineProperty(window.location, "reload", original);
      else delete (window.location as { reload?: unknown }).reload;
    }
  });

  test("App renders the invite page at /invite without asking who is signed in", async () => {
    const calls = installFetch([
      inspectRoute(validInvite),
      route("GET", "/api/auth/me", { status: 401, body: { error: "Unauthorized" } }),
    ]);
    visit(`/invite#${TOKEN}`);
    render(<App />);

    await waitFor(() => expect(screen.getByRole("heading", { name: "Welcome, Amy" })).toBeTruthy());
    expect(paths(calls)).not.toContain("/api/auth/me");
    expect(screen.queryByRole("heading", { name: "Sign in" })).toBeNull();
  });

  test("App also routes /invite/ with a trailing slash to the invite page", async () => {
    const calls = installFetch([inspectRoute(validInvite)]);
    visit(`/invite/#${TOKEN}`);
    render(<App />);

    await waitFor(() => expect(screen.getByRole("heading", { name: "Welcome, Amy" })).toBeTruthy());
    expect(calls[0]?.body).toEqual({ token: TOKEN });
    expect(window.location.hash).toBe("");
  });

  test("isInvitePath matches only the invite route", () => {
    for (const path of ["/invite", "/invite/"]) {
      visit(path);
      expect(isInvitePath()).toBe(true);
    }
    for (const path of ["/", "/invites", "/invite/extra", "/users"]) {
      visit(path);
      expect(isInvitePath()).toBe(false);
    }
  });
});

describe("hasOpenInvite", () => {
  const now = Date.UTC(2026, 10, 4, 10, 0, 0);

  test("is true only for an unexpired invite", () => {
    expect(hasOpenInvite(userRecord({ status: "invited", invite_expires_at: "2026-11-05 10:00:00" }), now)).toBe(true);
    expect(hasOpenInvite(userRecord({ status: "invited", invite_expires_at: "2026-11-04 09:59:59" }), now)).toBe(false);
    expect(hasOpenInvite(userRecord({ status: "invited", invite_expires_at: null }), now)).toBe(false);
  });
});

describe("describeInviteExpiry", () => {
  const now = Date.UTC(2026, 10, 4, 10, 0, 0);

  test("describes the remaining time, expiry and a missing invite", () => {
    expect(describeInviteExpiry("2026-11-10 22:00:00", now)).toBe("expires in 7 days");
    expect(describeInviteExpiry("2026-11-04 15:00:00", now)).toBe("expires in 5 hours");
    expect(describeInviteExpiry("2026-11-04 10:00:20", now)).toBe("expires in 1 minute");
    expect(describeInviteExpiry("2026-11-04 09:59:59", now)).toBe("expired");
    expect(describeInviteExpiry(null, now)).toBe("no open invite");
  });
});

describe("Users page invites", () => {
  const originalFetch = globalThis.fetch;
  const originalConfirm = globalThis.confirm;
  const originalClipboard = Object.getOwnPropertyDescriptor(navigator, "clipboard");

  beforeEach(() => {
    globalThis.confirm = () => true;
  });

  afterEach(() => {
    cleanup();
    globalThis.fetch = originalFetch;
    globalThis.confirm = originalConfirm;
    if (originalClipboard) Object.defineProperty(navigator, "clipboard", originalClipboard);
    else delete (navigator as { clipboard?: unknown }).clipboard;
  });

  const invited = userRecord({ status: "invited", invite_expires_at: sqliteTimestamp(6 * DAY_MS + 60_000) });
  const lapsed = userRecord({ id: 8, email: "bob@example.com", name: "Bob", status: "invited", invite_expires_at: sqliteTimestamp(-DAY_MS) });
  const active = userRecord({ id: 9, email: "cat@example.com", name: "Cat" });

  function directoryRoutes(users: UserRecord[]): Route[] {
    return [
      route("GET", "/api/auth/me", { status: 200, body: admin }),
      route("GET", "/api/users", { status: 200, body: users }),
      route("GET", "/api/identities", { status: 200, body: users.map((user) => userIdentities(user.id, user.name)) }),
    ];
  }

  test("marks invited users with their invite expiry", async () => {
    installFetch(directoryRoutes([invited, lapsed, active]));
    render(<UsersPage />, { wrapper: Wrapper });

    await waitFor(() => expect(screen.getByText("Amy")).toBeTruthy());
    expect(screen.getAllByText("Invited")).toHaveLength(2);
    expect(screen.getByText("expires in 6 days")).toBeTruthy();
    expect(screen.getByText("expired")).toBeTruthy();
    expect(screen.getAllByTitle("Copy invite link")).toHaveLength(2);
    expect(screen.getAllByTitle("Revoke invite")).toHaveLength(2);
    const catRow = screen.getByText("Cat").closest("tr");
    expect(catRow?.textContent).not.toContain("Invited");
  });

  test("Copy invite link issues a new invite and copies its absolute URL", async () => {
    const writeText = stubClipboard(() => Promise.resolve());
    const calls = installFetch([
      route("POST", "/api/users/7/invite", {
        status: 201,
        body: { invite_link: "/invite#cldi_fresh", invite_url: null, expires_at: sqliteTimestamp(7 * DAY_MS) },
      }),
      ...directoryRoutes([invited]),
    ]);
    render(<UsersPage />, { wrapper: Wrapper });
    await waitFor(() => expect(screen.getByTitle("Copy invite link")).toBeTruthy());

    fireEvent.click(screen.getByTitle("Copy invite link"));

    await waitFor(() => expect(writeText).toHaveBeenCalledWith("http://localhost/invite#cldi_fresh"));
    expect(calls.some((call) => call.method === "POST" && call.url.endsWith("/api/users/7/invite"))).toBe(true);
    const notice = screen.getByRole("status");
    expect(notice.textContent).toContain("Copied to clipboard.");
    expect(notice.textContent).toContain("Any previous invite link for this user no longer works.");
    expect(notice.textContent).toContain("http://localhost/invite#cldi_fresh");
  });

  test("Copy invite link asks before replacing an open invite and does nothing when declined", async () => {
    const prompts: string[] = [];
    globalThis.confirm = (message?: string) => {
      prompts.push(message ?? "");
      return false;
    };
    const writeText = stubClipboard(() => Promise.resolve());
    const calls = installFetch(directoryRoutes([invited]));
    render(<UsersPage />, { wrapper: Wrapper });
    await waitFor(() => expect(screen.getByTitle("Copy invite link")).toBeTruthy());

    fireEvent.click(screen.getByTitle("Copy invite link"));

    expect(prompts).toEqual(["Create a new invite link for Amy? The link sent earlier stops working."]);
    await Promise.resolve();
    expect(calls.some((call) => call.method === "POST")).toBe(false);
    expect(writeText).not.toHaveBeenCalled();
    expect(screen.queryByRole("status")).toBeNull();
  });

  test("Copy invite link reissues a lapsed invite without asking", async () => {
    const prompts: string[] = [];
    globalThis.confirm = (message?: string) => {
      prompts.push(message ?? "");
      return true;
    };
    const writeText = stubClipboard(() => Promise.resolve());
    installFetch([
      route("POST", "/api/users/8/invite", {
        status: 201,
        body: { invite_link: "/invite#cldi_bob", invite_url: null, expires_at: sqliteTimestamp(7 * DAY_MS) },
      }),
      ...directoryRoutes([lapsed]),
    ]);
    render(<UsersPage />, { wrapper: Wrapper });
    await waitFor(() => expect(screen.getByTitle("Copy invite link")).toBeTruthy());

    fireEvent.click(screen.getByTitle("Copy invite link"));

    await waitFor(() => expect(writeText).toHaveBeenCalledWith("http://localhost/invite#cldi_bob"));
    expect(prompts).toEqual([]);
    expect(screen.getByRole("status").textContent).not.toContain("Any previous invite link");
  });

  test("Copy invite link falls back to showing the link when the clipboard is denied", async () => {
    stubClipboard(() => Promise.reject(new Error("denied")));
    installFetch([
      route("POST", "/api/users/7/invite", {
        status: 201,
        body: { invite_link: "/invite#cldi_fresh", invite_url: "https://claudear.example.com/invite#cldi_fresh", expires_at: sqliteTimestamp(7 * DAY_MS) },
      }),
      ...directoryRoutes([invited]),
    ]);
    render(<UsersPage />, { wrapper: Wrapper });
    await waitFor(() => expect(screen.getByTitle("Copy invite link")).toBeTruthy());

    fireEvent.click(screen.getByTitle("Copy invite link"));

    await waitFor(() => expect(screen.getByRole("status").textContent).toContain("copy the link below manually"));
    expect(screen.getByText("https://claudear.example.com/invite#cldi_fresh")).toBeTruthy();
  });

  test("Revoke invite deletes the open invite", async () => {
    const calls = installFetch([
      route("DELETE", "/api/users/7/invite", { status: 200, body: { message: "Invite revoked" } }),
      ...directoryRoutes([invited]),
    ]);
    render(<UsersPage />, { wrapper: Wrapper });
    await waitFor(() => expect(screen.getByTitle("Revoke invite")).toBeTruthy());

    fireEvent.click(screen.getByTitle("Revoke invite"));

    await waitFor(() =>
      expect(calls.some((call) => call.method === "DELETE" && call.url.endsWith("/api/users/7/invite"))).toBe(true),
    );
  });

  test("creating a user without a password shows the invite link and opens their identities", async () => {
    const created = {
      ...userRecord({ id: 12, email: "dan@example.com", name: "Dan", status: "invited", invite_expires_at: sqliteTimestamp(7 * DAY_MS) }),
      invite: { invite_link: "/invite#cldi_new", invite_url: null, expires_at: sqliteTimestamp(7 * DAY_MS) },
    };
    const calls = installFetch([
      route("POST", "/api/users", { status: 201, body: created }),
      route("GET", "/api/users/12/identities", { status: 200, body: userIdentities(12, "Dan") }),
      route("GET", "/api/users/12/tokens", { status: 200, body: [] }),
      ...directoryRoutes([active]),
    ]);
    render(<UsersPage />, { wrapper: Wrapper });
    await waitFor(() => expect(screen.getByText("Add User")).toBeTruthy());

    fireEvent.click(screen.getByText("Add User"));
    const password = screen.getByLabelText("Password") as HTMLInputElement;
    expect(password.required).toBe(false);
    expect(password.placeholder).toBe("Leave blank to invite");
    fireEvent.change(screen.getByLabelText("Name"), { target: { value: "Dan" } });
    fireEvent.change(screen.getByLabelText("Email"), { target: { value: "dan@example.com" } });
    fireEvent.submit(screen.getByRole("button", { name: "Create" }));

    await waitFor(() => expect(screen.getByRole("status").textContent).toContain("http://localhost/invite#cldi_new"));
    const create = calls.find((call) => call.method === "POST" && call.url.endsWith("/api/users"));
    expect(create?.body).toEqual({ email: "dan@example.com", name: "Dan", role: "viewer" });
    expect(screen.getByRole("status").textContent).toContain("Send this link to Dan");
    expect(screen.getByRole("heading", { name: "Edit User" })).toBeTruthy();
    await waitFor(() => expect(screen.getByLabelText("Person key")).toBeTruthy());
    expect(paths(calls)).toContain("/api/users/12/identities");
  });
});

describe("Users page errors", () => {
  const originalFetch = globalThis.fetch;
  const originalConfirm = globalThis.confirm;

  beforeEach(() => {
    globalThis.confirm = () => true;
  });

  afterEach(() => {
    cleanup();
    globalThis.fetch = originalFetch;
    globalThis.confirm = originalConfirm;
  });

  const amy = userRecord();

  function directoryRoutes(): Route[] {
    return [
      route("GET", "/api/auth/me", { status: 200, body: admin }),
      route("GET", "/api/users", { status: 200, body: [amy] }),
      route("GET", "/api/identities", { status: 200, body: [userIdentities(amy.id, amy.name)] }),
      route("GET", `/api/users/${amy.id}/identities`, { status: 200, body: userIdentities(amy.id, amy.name) }),
      route("GET", `/api/users/${amy.id}/tokens`, { status: 200, body: [] }),
    ];
  }

  async function openEditForm() {
    render(<UsersPage />, { wrapper: Wrapper });
    await waitFor(() => expect(screen.getByTitle("Edit")).toBeTruthy());
    fireEvent.click(screen.getByTitle("Edit"));
    await waitFor(() => expect(screen.getByRole("heading", { name: "Edit User" })).toBeTruthy());
  }

  test("shows the server message when an update is rejected", async () => {
    installFetch([
      route("PUT", `/api/users/${amy.id}`, { status: 409, body: { error: "A user with this email already exists" } }),
      ...directoryRoutes(),
    ]);
    await openEditForm();

    const [loginEmail] = screen.getAllByLabelText("Email");
    fireEvent.change(loginEmail, { target: { value: "bob@example.com" } });
    fireEvent.submit(screen.getByRole("button", { name: "Update" }));

    expect(await screen.findByText("A user with this email already exists")).toBeTruthy();
  });

  test("rejects a password over 72 UTF-8 bytes before calling the server", async () => {
    const calls = installFetch(directoryRoutes());
    await openEditForm();

    fireEvent.change(screen.getByLabelText("Password"), { target: { value: "\u{1F600}".repeat(19) } });
    fireEvent.submit(screen.getByRole("button", { name: "Update" }));

    expect(await screen.findByText("Password must be 8-72 bytes")).toBeTruthy();
    expect(calls.some((call) => call.method === "PUT")).toBe(false);
  });

  test("shows the server message when a delete is rejected", async () => {
    installFetch([
      route("DELETE", `/api/users/${amy.id}`, { status: 403, body: { error: "Admin access required" } }),
      ...directoryRoutes(),
    ]);
    render(<UsersPage />, { wrapper: Wrapper });
    await waitFor(() => expect(screen.getByTitle("Delete")).toBeTruthy());

    fireEvent.click(screen.getByTitle("Delete"));

    expect(await screen.findByText("Admin access required")).toBeTruthy();
  });
});
