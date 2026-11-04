import { afterEach, describe, expect, mock, test } from "bun:test";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SWRConfig } from "swr";
import type { ReactNode } from "react";
import { AuthProvider } from "../src/lib/auth";
import type { ConfigResponse } from "../src/lib/api";

type Role = "admin" | "viewer";
type FetchCall = { url: string; method: string; body?: unknown };
type PutReply = { status: number; data: unknown };

const TRACKS_CONFIG = [
  'workspace = "/tmp/work"',
  "known_orgs = [",
  '    "appwrite",',
  '    "utopia-php",',
  "]",
  "",
  "[scm.github]",
  'token = "[REDACTED]"',
  'webhook_secret = ""',
  "",
  "[chat]",
  'system_prompt = """',
  "You are helpful.",
  "Answer briefly.",
  '"""',
  "",
  "[deploy_qa]",
  "enabled = true",
  "",
  "[[deploy_qa.tracks]]",
  'name = "cloud"',
  'repo = "appwrite/cloud"',
  'tag_filter = "any"',
  "",
  "[[deploy_qa.tracks]]",
  'name = "edge"',
  'repo = "appwrite/edge"',
  'tag_filter = "any"',
  "",
].join("\n");

const FIRST_TRACK = ['[[deploy_qa.tracks]]', 'name = "cloud"', 'repo = "appwrite/cloud"', 'tag_filter = "any"'].join("\n");

function configResponse(overrides: Partial<ConfigResponse> = {}): ConfigResponse {
  return {
    content: TRACKS_CONFIG,
    secret_paths: ["scm.github.token", "scm.github.webhook_secret"],
    runtime_overrides: [],
    restart_required: false,
    writable: true,
    revision: "rev-1",
    optional_sections: [],
    ...overrides,
  };
}

function jsonResponse(status: number, data: unknown) {
  return Promise.resolve({
    ok: status >= 200 && status < 300,
    status,
    statusText: status >= 200 && status < 300 ? "OK" : "Error",
    json: () => Promise.resolve(data),
  });
}

function createHarness(role: Role, initial: ConfigResponse = configResponse()) {
  let config = initial;
  const calls: FetchCall[] = [];
  const putReplies: PutReply[] = [];

  globalThis.fetch = mock((input: string | URL | Request, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
    const method = init?.method ?? "GET";
    const body = typeof init?.body === "string" ? JSON.parse(init.body) : undefined;
    calls.push({ url, method, body });

    if (url.endsWith("/api/auth/me")) {
      return jsonResponse(200, { id: 1, email: `${role}@test.com`, name: role, role, avatar_url: null });
    }
    if (url.endsWith("/api/instructions/global")) {
      return jsonResponse(200, { scope: "global", repo: null, text: "", updated_at: null });
    }
    if (url.endsWith("/api/config") && method === "GET") {
      return jsonResponse(200, config);
    }
    if (url.endsWith("/api/config") && method === "PUT") {
      const reply = putReplies.shift();
      if (reply) return jsonResponse(reply.status, reply.data);
      config = { ...config, content: (body as { content: string }).content, revision: "rev-2", restart_required: true };
      return jsonResponse(200, {
        ok: true,
        restart_required: true,
        revision: "rev-2",
        message: "Config saved. Restart claudear to apply changes.",
      });
    }
    return jsonResponse(200, {});
  }) as unknown as typeof fetch;

  return {
    calls,
    configPuts: () => calls.filter((call) => call.url.endsWith("/api/config") && call.method === "PUT"),
    replyToNextPut(reply: PutReply) {
      putReplies.push(reply);
    },
    setConfig(next: ConfigResponse) {
      config = next;
    },
  };
}

function Wrapper({ children }: { children: ReactNode }) {
  return (
    <SWRConfig value={{ provider: () => new Map(), dedupingInterval: 0 }}>
      <AuthProvider>{children}</AuthProvider>
    </SWRConfig>
  );
}

async function renderConfigPage() {
  const ConfigPage = (await import("../src/pages/config")).default;
  render(<ConfigPage />, { wrapper: Wrapper });
  await waitFor(() => {
    expect(screen.getByLabelText("deploy_qa.tracks[1].repo")).toBeTruthy();
  });
}

function saveButton() {
  return screen.getByRole("button", { name: "Save" }) as HTMLButtonElement;
}

describe("ConfigPage", () => {
  const originalFetch = globalThis.fetch;

  afterEach(() => {
    cleanup();
    globalThis.fetch = originalFetch;
  });

  test("renders each [[deploy_qa.tracks]] entry as its own card and edits only that occurrence", async () => {
    const harness = createHarness("admin");
    await renderConfigPage();

    expect(screen.getByText("Deploy QA Track #1")).toBeTruthy();
    expect(screen.getByText("Deploy QA Track #2")).toBeTruthy();
    expect((screen.getByLabelText("deploy_qa.tracks[0].repo") as HTMLInputElement).value).toBe("appwrite/cloud");

    fireEvent.change(screen.getByLabelText("deploy_qa.tracks[1].repo"), { target: { value: "appwrite/edge-next" } });
    expect(saveButton().disabled).toBe(false);
    fireEvent.click(saveButton());

    await waitFor(() => {
      expect(harness.configPuts().length).toBe(1);
    });
    const body = harness.configPuts()[0].body as { content: string; revision: string };
    expect(body.revision).toBe("rev-1");
    expect(body.content).toContain(FIRST_TRACK);
    expect(body.content).toContain('name = "edge"\nrepo = "appwrite/edge-next"\ntag_filter = "any"');
    expect(body.content).not.toContain('repo = "appwrite/edge"\n');
    expect(body.content.replace('repo = "appwrite/edge-next"', 'repo = "appwrite/edge"')).toBe(TRACKS_CONFIG);

    await waitFor(() => {
      expect(screen.getByText("Config saved. Restart claudear to apply changes.")).toBeTruthy();
    });
    expect(screen.getByText(/Restart required/)).toBeTruthy();
  });

  test("edits a multi-line array without breaking the following fields", async () => {
    const harness = createHarness("admin");
    await renderConfigPage();

    expect(screen.getByText("appwrite")).toBeTruthy();
    expect(screen.getByText("utopia-php")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Remove appwrite" }));
    fireEvent.click(saveButton());

    await waitFor(() => {
      expect(harness.configPuts().length).toBe(1);
    });
    const content = (harness.configPuts()[0].body as { content: string }).content;
    expect(content.startsWith('workspace = "/tmp/work"\nknown_orgs = ["utopia-php"]\n\n[scm.github]\n')).toBe(true);
  });

  test("never renders a secret path's value and restores the sentinel when the edit is cleared", async () => {
    createHarness(
      "admin",
      configResponse({ content: TRACKS_CONFIG.replace('webhook_secret = ""', 'webhook_secret = "leaked-hook-secret"') })
    );
    await renderConfigPage();

    expect(document.body.innerHTML).not.toContain("leaked-hook-secret");
    expect(document.body.innerHTML).not.toContain("[REDACTED]");

    const token = screen.getByLabelText("scm.github.token") as HTMLInputElement;
    expect(token.type).toBe("password");
    expect(token.value).toBe("");
    expect(token.placeholder).toBe("Set (hidden), type to replace");

    fireEvent.change(token, { target: { value: "ghp_new" } });
    expect((screen.getByLabelText("scm.github.token") as HTMLInputElement).value).toBe("ghp_new");
    expect(saveButton().disabled).toBe(false);

    fireEvent.change(screen.getByLabelText("scm.github.token"), { target: { value: "" } });
    expect(saveButton().disabled).toBe(true);

    fireEvent.click(screen.getByRole("button", { name: "Raw" }));
    expect((screen.getByLabelText("claudear.toml") as HTMLTextAreaElement).value).toContain('token = "[REDACTED]"');
  });

  test("shows an unset secret as not set", async () => {
    createHarness("admin");
    await renderConfigPage();

    const webhookSecret = screen.getByLabelText("scm.github.webhook_secret") as HTMLInputElement;
    expect(webhookSecret.type).toBe("password");
    expect(webhookSecret.placeholder).toBe("Not set");
  });

  test("matches server secret paths for quoted keys and tables nested under an array entry", async () => {
    const content = [
      TRACKS_CONFIG,
      "[deploy_qa.tracks.notify]",
      'webhook = "[REDACTED]"',
      "",
      "[agent.providers.claude.env]",
      '"OPENAI_KEY" = "[REDACTED]"',
      "",
    ].join("\n");
    createHarness(
      "admin",
      configResponse({
        content,
        secret_paths: [
          ...configResponse().secret_paths,
          "agent.providers.claude.env.OPENAI_KEY",
          "deploy_qa.tracks[1].notify.webhook",
        ],
      })
    );
    await renderConfigPage();

    for (const path of ["agent.providers.claude.env.OPENAI_KEY", "deploy_qa.tracks[1].notify.webhook"]) {
      const input = screen.getByLabelText(path) as HTMLInputElement;
      expect(input.type).toBe("password");
      expect(input.value).toBe("");
    }
    expect(document.body.innerHTML).not.toContain("[REDACTED]");
  });

  test("shows the server validation message when a save is rejected", async () => {
    const harness = createHarness("admin");
    harness.replyToNextPut({ status: 400, data: { error: "Config validation failed: X", code: "invalid" } });
    await renderConfigPage();

    fireEvent.change(screen.getByLabelText("deploy_qa.tracks[0].name"), { target: { value: "cloud-2" } });
    fireEvent.click(saveButton());

    await waitFor(() => {
      expect(screen.getByText("Config validation failed: X")).toBeTruthy();
    });
    expect(screen.queryByText(/Restart required/)).toBeNull();
  });

  test("prompts a reload when the file changed on disk", async () => {
    const harness = createHarness("admin");
    harness.replyToNextPut({
      status: 409,
      data: { error: "claudear.toml changed since it was loaded", code: "revision_mismatch" },
    });
    await renderConfigPage();

    fireEvent.change(screen.getByLabelText("deploy_qa.tracks[0].name"), { target: { value: "cloud-2" } });
    fireEvent.click(saveButton());

    await waitFor(() => {
      expect(screen.getByRole("button", { name: "Reload" })).toBeTruthy();
    });

    harness.setConfig(configResponse({ content: TRACKS_CONFIG.replace('name = "edge"', 'name = "edge-remote"'), revision: "rev-9" }));
    fireEvent.click(screen.getByRole("button", { name: "Reload" }));

    await waitFor(() => {
      expect((screen.getByLabelText("deploy_qa.tracks[1].name") as HTMLInputElement).value).toBe("edge-remote");
    });
    expect((screen.getByLabelText("deploy_qa.tracks[0].name") as HTMLInputElement).value).toBe("cloud");
    expect(screen.queryByRole("button", { name: "Reload" })).toBeNull();
  });

  test("disables Save with an explanation when the file is not writable", async () => {
    createHarness("admin", configResponse({ writable: false }));
    await renderConfigPage();

    fireEvent.change(screen.getByLabelText("deploy_qa.tracks[0].name"), { target: { value: "cloud-2" } });

    expect(saveButton().disabled).toBe(true);
    expect(screen.getByText(/Saving is disabled/)).toBeTruthy();
  });

  test("shows a persistent restart banner when the running config differs from disk", async () => {
    createHarness("admin", configResponse({ restart_required: true, runtime_overrides: ["scm.github.token"] }));
    await renderConfigPage();

    expect(screen.getByText(/Restart required/)).toBeTruthy();
    expect(
      screen.getByText("overridden at runtime (environment variable or instructions_file); the running value differs")
    ).toBeTruthy();
    expect(screen.getByText(/comments are removed and defaults written out/)).toBeTruthy();
  });

  test("Add section appends the template", async () => {
    const template = '[issues.linear]\napi_key = ""\nteam_id = "ENG"';
    createHarness(
      "admin",
      configResponse({
        optional_sections: [
          { key: "issues.linear", template },
          { key: "deploy_qa", template: "[deploy_qa]\nenabled = false" },
        ],
      })
    );
    await renderConfigPage();

    const select = screen.getByLabelText("Add section") as HTMLSelectElement;
    expect([...select.options].map((option) => option.value)).toEqual(["", "issues.linear"]);

    fireEvent.change(select, { target: { value: "issues.linear" } });

    await waitFor(() => {
      expect(screen.getByText("Issue Source: linear")).toBeTruthy();
    });
    expect((screen.getByLabelText("issues.linear.team_id") as HTMLInputElement).value).toBe("ENG");
    expect(screen.queryByLabelText("Add section")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Raw" }));
    expect((screen.getByLabelText("claudear.toml") as HTMLTextAreaElement).value).toBe(
      `${TRACKS_CONFIG.trimEnd()}\n\n${template}\n`
    );
  });

  test("multi-line strings are read-only and point at the Raw view", async () => {
    createHarness("admin");
    await renderConfigPage();

    expect(screen.queryByLabelText("chat.system_prompt")).toBeNull();
    expect(screen.getByText(/You are helpful\./)).toBeTruthy();

    fireEvent.click(screen.getAllByRole("button", { name: "Edit in Raw view" })[0]);
    expect(screen.getByLabelText("claudear.toml")).toBeTruthy();
  });

  test("links the deploy_qa card to the Deploy QA page", async () => {
    createHarness("admin");
    await renderConfigPage();

    expect(screen.getByRole("button", { name: "Open Deploy QA" })).toBeTruthy();
  });

  test("a viewer makes no config or global instruction requests", async () => {
    const harness = createHarness("viewer");
    const ConfigPage = (await import("../src/pages/config")).default;
    render(<ConfigPage />, { wrapper: Wrapper });

    await waitFor(() => {
      expect(harness.calls.some((call) => call.url.endsWith("/api/auth/me"))).toBe(true);
    });
    await waitFor(() => {
      expect(screen.getByText(/Access denied/)).toBeTruthy();
    });
    await new Promise((resolve) => setTimeout(resolve, 20));

    expect(harness.calls.some((call) => call.url.includes("/api/config"))).toBe(false);
    expect(harness.calls.some((call) => call.url.includes("/api/instructions/global"))).toBe(false);
  });
});
