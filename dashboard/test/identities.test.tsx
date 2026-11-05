import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SWRConfig } from "swr";
import type { ReactNode } from "react";
import { AuthProvider } from "../src/lib/auth";
import { IdentityForm } from "../src/components/users/identity-form";
import UsersPage from "../src/pages/users";
import SettingsPage from "../src/pages/settings";
import type { Identities, UserIdentities } from "../src/lib/api";

type Call = { url: string; method: string; body?: unknown };
type Reply = { status: number; body?: unknown };
type Route = (call: Call) => Reply | undefined;

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

function userIdentities(overrides: Partial<UserIdentities> = {}, identities: Partial<Identities> = {}): UserIdentities {
  return {
    user_id: 3,
    name: "Jake",
    login_email: "jake@appwrite.io",
    person_key: "jake",
    slug: null,
    origin: "config",
    status: "active",
    redacted: false,
    identities: { ...emptyIdentities, ...identities },
    updated_at: null,
    ...overrides,
  };
}

const admin = { id: 1, email: "admin@test.com", name: "Admin", role: "admin" };
const viewer = { id: 3, email: "jake@appwrite.io", name: "Jake", role: "viewer" };

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

function me(user: typeof admin): Route {
  return route("GET", "/api/auth/me", { status: 200, body: user });
}

function Wrapper({ children }: { children: ReactNode }) {
  return <SWRConfig value={{ provider: () => new Map(), dedupingInterval: 0 }}>{children}</SWRConfig>;
}

function AuthWrapper({ children }: { children: ReactNode }) {
  return (
    <Wrapper>
      <AuthProvider>{children}</AuthProvider>
    </Wrapper>
  );
}

function input(label: string) {
  return screen.getByLabelText(label) as HTMLInputElement;
}

describe("IdentityForm", () => {
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

  test("prefills values inherited from config", async () => {
    installFetch([
      route("GET", "/api/users/3/identities", {
        status: 200,
        body: userIdentities({}, { github_usernames: ["jakebarnby", "jake-alt"], discord_id: "452316113016193024" }),
      }),
    ]);

    render(<IdentityForm userId={3} canEditSlug />, { wrapper: Wrapper });

    await waitFor(() => expect(screen.getByText("From config (jake)")).toBeTruthy());
    expect(input("GitHub").value).toBe("jakebarnby, jake-alt");
    expect(input("Discord ID").value).toBe("452316113016193024");
    expect(input("Person key").value).toBe("");
    expect(input("Person key").placeholder).toBe("jake");
    expect(screen.queryByRole("button", { name: /Revert to config/ })).toBeNull();
  });

  test("binds the editable input to the admin-set slug, never the derived person key", async () => {
    const calls = installFetch([
      route("GET", "/api/users/3/identities", {
        status: 200,
        body: userIdentities({ origin: "console", person_key: "console-3", slug: null, updated_at: "2026-11-04 10:00:00" }),
      }),
      route("PUT", "/api/users/3/identities", (call) => ({
        status: 200,
        body: userIdentities(
          { origin: "console", person_key: "console-3", slug: null, updated_at: "2026-11-04 11:00:00" },
          call.body as Partial<Identities>,
        ),
      })),
    ]);

    render(<IdentityForm userId={3} canEditSlug />, { wrapper: Wrapper });
    await waitFor(() => expect(input("Person key")).toBeTruthy());

    expect(input("Person key").value).toBe("");
    expect(input("Person key").readOnly).toBe(false);
    expect(screen.getByText("console-3")).toBeTruthy();

    fireEvent.change(input("GitHub"), { target: { value: "jakebarnby" } });
    fireEvent.click(screen.getByRole("button", { name: "Save identities" }));

    await waitFor(() => expect(screen.getByText("Identities saved")).toBeTruthy());
    expect(calls.find((call) => call.method === "PUT")?.body).toEqual({ github_usernames: ["jakebarnby"] });
  });

  test("shows a non-admin their derived person key read-only", async () => {
    installFetch([
      route("GET", "/api/users/3/identities", {
        status: 200,
        body: userIdentities({ origin: "console", person_key: "console-3", slug: null, updated_at: "2026-11-04 10:00:00" }),
      }),
    ]);

    render(<IdentityForm userId={3} canEditSlug={false} />, { wrapper: Wrapper });
    await waitFor(() => expect(input("Person key")).toBeTruthy());

    expect(input("Person key").value).toBe("console-3");
    expect(input("Person key").readOnly).toBe(true);
  });

  test("sends only the fields that changed", async () => {
    const calls = installFetch([
      route("GET", "/api/users/3/identities", {
        status: 200,
        body: userIdentities({}, { github_usernames: ["jakebarnby"], slack_id: "U0123456789" }),
      }),
      route("PUT", "/api/users/3/identities", (call) => ({
        status: 200,
        body: userIdentities(
          { origin: "console", updated_at: "2026-11-04 10:00:00" },
          { github_usernames: ["jakebarnby"], slack_id: "U0123456789", ...(call.body as Partial<Identities>) },
        ),
      })),
    ]);

    render(<IdentityForm userId={3} canEditSlug />, { wrapper: Wrapper });
    await waitFor(() => expect(input("Discord ID")).toBeTruthy());

    const save = screen.getByRole("button", { name: "Save identities" }) as HTMLButtonElement;
    expect(save.disabled).toBe(true);

    fireEvent.change(input("Discord ID"), { target: { value: " 452316113016193024 " } });
    fireEvent.change(input("Jira"), { target: { value: "jake.barnby, , jb" } });
    fireEvent.change(input("Slack ID"), { target: { value: "" } });
    fireEvent.change(input("GitHub"), { target: { value: "jakebarnby " } });
    fireEvent.click(save);

    await waitFor(() => expect(screen.getByText("Identities saved")).toBeTruthy());
    const puts = calls.filter((call) => call.method === "PUT");
    expect(puts).toHaveLength(1);
    expect(puts[0].body).toEqual({
      discord_id: "452316113016193024",
      jira_usernames: ["jake.barnby", "jb"],
      slack_id: "",
    });
    expect(screen.getByText("Console")).toBeTruthy();
  });

  test("shows violations next to the offending field", async () => {
    installFetch([
      route("GET", "/api/users/3/identities", { status: 200, body: userIdentities() }),
      route("PUT", "/api/users/3/identities", {
        status: 400,
        body: {
          error: "Invalid identities",
          violations: [{ field: "discord_id", message: "Discord ID must be a 17-20 digit Discord user ID" }],
        },
      }),
    ]);

    render(<IdentityForm userId={3} canEditSlug />, { wrapper: Wrapper });
    await waitFor(() => expect(input("Discord ID")).toBeTruthy());

    fireEvent.change(input("Discord ID"), { target: { value: "jake#1234" } });
    fireEvent.click(screen.getByRole("button", { name: "Save identities" }));

    const message = await screen.findByText("Discord ID must be a 17-20 digit Discord user ID");
    expect(screen.getByText("Invalid identities")).toBeTruthy();
    const discord = input("Discord ID");
    expect(discord.getAttribute("aria-invalid")).toBe("true");
    expect(message.parentElement?.id).toBe(discord.getAttribute("aria-describedby") ?? "");
    expect(discord.parentElement?.contains(message)).toBe(true);
    expect(input("Slack ID").getAttribute("aria-invalid")).toBeNull();
    expect(discord.value).toBe("jake#1234");
  });

  test("disables redacted fields and never sends them", async () => {
    const calls = installFetch([
      route("GET", "/api/users/3/identities", {
        status: 200,
        body: userIdentities({ redacted: true, login_email: null }, { github_usernames: ["jakebarnby"] }),
      }),
      route("PUT", "/api/users/3/identities", { status: 200, body: userIdentities({ redacted: true }) }),
    ]);

    render(<IdentityForm userId={3} canEditSlug={false} />, { wrapper: Wrapper });
    await waitFor(() => expect(input("Email")).toBeTruthy());

    for (const label of ["Email", "Pushover key", "SMS", "WhatsApp", "Telegram chat ID"]) {
      const field = input(label);
      expect(field.disabled).toBe(true);
      expect(field.placeholder).toBe("Hidden");
    }
    expect(input("GitHub").disabled).toBe(false);
    expect(input("Discord ID").disabled).toBe(false);

    fireEvent.change(input("GitHub"), { target: { value: "jakebarnby, jb" } });
    fireEvent.click(screen.getByRole("button", { name: "Save identities" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true));
    expect(calls.find((call) => call.method === "PUT")?.body).toEqual({ github_usernames: ["jakebarnby", "jb"] });
  });

  test("keeps the person key read-only when the slug cannot be edited", async () => {
    installFetch([route("GET", "/api/users/3/identities", { status: 200, body: userIdentities() })]);

    render(<IdentityForm userId={3} canEditSlug={false} />, { wrapper: Wrapper });
    await waitFor(() => expect(input("Person key")).toBeTruthy());

    expect(input("Person key").value).toBe("jake");
    expect(input("Person key").readOnly).toBe(true);
    expect(screen.getByText("Only admins can change the person key.")).toBeTruthy();
  });

  test("sends a changed slug when the slug can be edited", async () => {
    const calls = installFetch([
      route("GET", "/api/users/3/identities", { status: 200, body: userIdentities({ origin: "unset", slug: null }) }),
      route("PUT", "/api/users/3/identities", {
        status: 200,
        body: userIdentities({ origin: "console", slug: "jake", updated_at: "2026-11-04 10:00:00" }),
      }),
    ]);

    render(<IdentityForm userId={3} canEditSlug />, { wrapper: Wrapper });
    await waitFor(() => expect(screen.getByText("Not set")).toBeTruthy());

    expect(input("Person key").readOnly).toBe(false);
    fireEvent.change(input("Person key"), { target: { value: "jake" } });
    fireEvent.click(screen.getByRole("button", { name: "Save identities" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true));
    expect(calls.find((call) => call.method === "PUT")?.body).toEqual({ slug: "jake" });
  });

  test("reverts a console override to config", async () => {
    const calls = installFetch([
      route("GET", "/api/users/3/identities", {
        status: 200,
        body: userIdentities({ origin: "console", updated_at: "2026-11-04 10:00:00" }, { discord_id: "452316113016193024" }),
      }),
      route("DELETE", "/api/users/3/identities", {
        status: 200,
        body: userIdentities({}, { github_usernames: ["jakebarnby"] }),
      }),
    ]);

    render(<IdentityForm userId={3} canEditSlug />, { wrapper: Wrapper });
    await waitFor(() => expect(screen.getByText("Console")).toBeTruthy());

    fireEvent.click(screen.getByRole("button", { name: /Revert to config/ }));

    await waitFor(() => expect(screen.getByText("From config (jake)")).toBeTruthy());
    expect(calls.filter((call) => call.method === "DELETE")).toHaveLength(1);
    expect(input("GitHub").value).toBe("jakebarnby");
    expect(input("Discord ID").value).toBe("");
    expect(screen.queryByRole("button", { name: /Revert to config/ })).toBeNull();
  });

  for (const [name, reverted] of [
    [
      "an admin-set slug the server keeps",
      userIdentities({ origin: "console", slug: "jake", updated_at: "2026-11-04 11:00:00" }),
    ],
    ["a slug equal to the config key the server drops with the row", userIdentities()],
  ] as const) {
    test(`tells a non-admin their person key stays for ${name}`, async () => {
      const prompts: string[] = [];
      globalThis.confirm = (message?: string) => {
        prompts.push(message ?? "");
        return true;
      };
      installFetch([
        route("GET", "/api/users/3/identities", {
          status: 200,
          body: userIdentities(
            { origin: "console", slug: "jake", updated_at: "2026-11-04 10:00:00" },
            { github_usernames: ["jakebarnby"] },
          ),
        }),
        route("DELETE", "/api/users/3/identities", { status: 200, body: reverted }),
      ]);

      render(<IdentityForm userId={3} canEditSlug={false} />, { wrapper: Wrapper });
      await waitFor(() => expect(screen.getByText("Console")).toBeTruthy());

      fireEvent.click(screen.getByRole("button", { name: /Revert to config/ }));

      await waitFor(() => expect(screen.getByText("Reverted to config; person key unchanged")).toBeTruthy());
      expect(prompts).toHaveLength(1);
      expect(prompts[0]).toContain('Your person key stays "jake".');
      expect(prompts[0]).not.toContain("is kept");
      expect(input("Person key").value).toBe("jake");
      expect(input("GitHub").value).toBe("");
    });
  }

  test("does not promise an admin that reverting keeps the slug", async () => {
    const prompts: string[] = [];
    globalThis.confirm = (message?: string) => {
      prompts.push(message ?? "");
      return false;
    };
    const calls = installFetch([
      route("GET", "/api/users/3/identities", {
        status: 200,
        body: userIdentities({ origin: "console", slug: "jake", updated_at: "2026-11-04 10:00:00" }),
      }),
    ]);

    render(<IdentityForm userId={3} canEditSlug />, { wrapper: Wrapper });
    await waitFor(() => expect(screen.getByText("Console")).toBeTruthy());

    fireEvent.click(screen.getByRole("button", { name: /Revert to config/ }));

    expect(prompts).toHaveLength(1);
    expect(prompts[0]).not.toContain("person key stays");
    expect(calls.some((call) => call.method === "DELETE")).toBe(false);
  });

  test("shows the server error when a handle belongs to someone else", async () => {
    installFetch([
      route("GET", "/api/users/3/identities", { status: 200, body: userIdentities() }),
      route("PUT", "/api/users/3/identities", {
        status: 409,
        body: { error: "GitHub login 'octocat' already belongs to another person" },
      }),
    ]);

    render(<IdentityForm userId={3} canEditSlug />, { wrapper: Wrapper });
    await waitFor(() => expect(input("GitHub")).toBeTruthy());

    fireEvent.change(input("GitHub"), { target: { value: "octocat" } });
    fireEvent.click(screen.getByRole("button", { name: "Save identities" }));

    expect(await screen.findByText("GitHub login 'octocat' already belongs to another person")).toBeTruthy();
  });
});

describe("Users page identities", () => {
  const originalFetch = globalThis.fetch;

  afterEach(() => {
    cleanup();
    globalThis.fetch = originalFetch;
  });

  test("admin sees an identities column and edits identities with the slug unlocked", async () => {
    const calls = installFetch([
      me(admin),
      route("GET", "/api/users", {
        status: 200,
        body: [
          { id: 3, email: "jake@appwrite.io", name: "Jake", role: "viewer", avatar_url: null, status: "active", invite_expires_at: null, created_at: "2026-11-01 10:00:00", updated_at: "2026-11-01 10:00:00" },
        ],
      }),
      route("GET", "/api/identities", {
        status: 200,
        body: [userIdentities({}, { github_usernames: ["jakebarnby"], discord_id: "452316113016193024" })],
      }),
      route("GET", "/api/users/3/identities", { status: 200, body: userIdentities({}, { github_usernames: ["jakebarnby"] }) }),
      route("GET", "/api/users/3/tokens", { status: 200, body: [] }),
    ]);

    render(<UsersPage />, { wrapper: AuthWrapper });

    await waitFor(() => expect(screen.getByText("@jakebarnby")).toBeTruthy());
    expect(screen.getByRole("columnheader", { name: "Identities" })).toBeTruthy();
    expect(screen.getByText("Discord")).toBeTruthy();
    expect(screen.getByTitle("Person key").textContent).toBe("jake");
    expect(calls.some((call) => call.url.endsWith("/api/identities"))).toBe(true);

    fireEvent.click(screen.getByTitle("Edit"));
    await waitFor(() => expect(input("Person key")).toBeTruthy());
    expect(input("Person key").readOnly).toBe(false);
    expect(screen.getByText("API Tokens")).toBeTruthy();
  });

  test("non-admin sees only their own identities", async () => {
    const calls = installFetch([
      me(viewer),
      route("GET", "/api/users/3/identities", { status: 200, body: userIdentities() }),
    ]);

    render(<UsersPage />, { wrapper: AuthWrapper });

    await waitFor(() => expect(screen.getByRole("heading", { name: "My identities" })).toBeTruthy());
    await waitFor(() => expect(input("Person key")).toBeTruthy());
    expect(input("Person key").readOnly).toBe(true);
    expect(screen.queryByText("Add User")).toBeNull();
    expect(screen.queryByRole("table")).toBeNull();
    expect(screen.queryByText(/permission/)).toBeNull();
    const paths = calls.map((call) => new URL(call.url, "http://localhost").pathname);
    expect(paths).not.toContain("/api/users");
    expect(paths).not.toContain("/api/identities");
    const viewerReadable = new Set(["/api/auth/me", "/api/users/3/identities"]);
    expect(paths.filter((path) => !viewerReadable.has(path))).toEqual([]);
  });
});

describe("Settings page identities", () => {
  const originalFetch = globalThis.fetch;

  afterEach(() => {
    cleanup();
    globalThis.fetch = originalFetch;
  });

  test("shows a My Identities card for the current user", async () => {
    const calls = installFetch([
      me(viewer),
      route("GET", "/api/users/3/identities", { status: 200, body: userIdentities({}, { github_usernames: ["jakebarnby"] }) }),
    ]);

    render(<SettingsPage />, { wrapper: AuthWrapper });

    await waitFor(() => expect(screen.getByText("My Identities")).toBeTruthy());
    await waitFor(() => expect(input("GitHub").value).toBe("jakebarnby"));
    expect(input("Person key").readOnly).toBe(true);
    expect(calls.some((call) => call.url.endsWith("/api/users/3/identities"))).toBe(true);
  });

  test("lets an admin edit their own person key", async () => {
    installFetch([
      me({ ...admin, id: 3 }),
      route("GET", "/api/users/3/identities", { status: 200, body: userIdentities() }),
    ]);

    render(<SettingsPage />, { wrapper: AuthWrapper });

    await waitFor(() => expect(input("Person key")).toBeTruthy());
    expect(input("Person key").readOnly).toBe(false);
  });
});
