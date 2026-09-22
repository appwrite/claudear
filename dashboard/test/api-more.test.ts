import { afterEach, describe, expect, mock, test } from "bun:test";
import {
  createExperiment,
  deleteUser,
  fetchConfig,
  fetchIssues,
  fetchRepoLearning,
  getUser,
  saveConfig,
  setOnUnauthorized,
  updateExperiment,
  updateProfile,
  updateUser,
  uploadAvatar,
} from "../src/lib/api";

function mockFetchResponse(options: {
  data?: unknown;
  status?: number;
}) {
  const status = options.status ?? 200;
  globalThis.fetch = mock((_input?: RequestInfo | URL, _init?: RequestInit) =>
    Promise.resolve({
      ok: status >= 200 && status < 300,
      status,
      statusText: status >= 200 && status < 300 ? "OK" : "Error",
      json: () => Promise.resolve(options.data),
      text: () => Promise.resolve(JSON.stringify(options.data ?? null)),
      headers: new Headers({ "content-type": "application/json" }),
    })
  ) as unknown as typeof fetch;
}

describe("additional api coverage", () => {
  const originalFetch = globalThis.fetch;

  afterEach(() => {
    globalThis.fetch = originalFetch;
    setOnUnauthorized(() => {});
  });

  test("fetchIssues builds query string with params", async () => {
    mockFetchResponse({ data: { issues: [], total: 0, page: 2, per_page: 50 } });
    await fetchIssues({ source: "linear", page: 2, per_page: 50 });
    expect(fetch).toHaveBeenCalledWith("/api/issues?source=linear&page=2&per_page=50");
  });

  test("create/update experiment and config/profile/user helpers send expected requests", async () => {
    const calls: Array<{ url: string; init?: RequestInit }> = [];
    globalThis.fetch = mock((input: string | URL | Request, init?: RequestInit) => {
      const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
      calls.push({ url, init });

      if (url.endsWith("/api/experiments") && init?.method === "POST") {
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () =>
            Promise.resolve({
              id: 1,
              experiment_name: "exp",
              variant: "control",
              prompt_template: "prompt",
              prompt_hash: "h",
              created_at: "2024-01-01T00:00:00Z",
              active: true,
              success_count: 1,
              failure_count: 0,
              avg_time_to_merge: null,
              avg_review_score: null,
            }),
        });
      }

      if (url.endsWith("/api/experiments/1")) {
        return Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve({ ok: true }) });
      }

      if (url.endsWith("/api/config") && !init?.method) {
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () => Promise.resolve({ content: "x", path: "/tmp/claudear.toml" }),
        });
      }

      if (url.endsWith("/api/config") && init?.method === "PUT") {
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () => Promise.resolve({ ok: true, message: "saved" }),
        });
      }

      if (url.includes("/api/repos/") && url.endsWith("/learning")) {
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () =>
            Promise.resolve({
              repo: "org/repo name",
              knowledge: [],
              knowledge_total: 0,
              instructions: [],
              review_patterns: [],
              review_pattern_summary: { total_patterns: 0, by_category: {}, promoted_count: 0 },
              strategies: [],
              diff_analyses: [],
              correlations: [],
            }),
        });
      }

      if (url.endsWith("/api/users/42") && !init?.method) {
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () =>
            Promise.resolve({
              id: 42,
              email: "u@test.com",
              name: "User",
              role: "viewer",
              created_at: "",
              updated_at: "",
            }),
        });
      }

      if (url.endsWith("/api/users/42") && init?.method === "PUT") {
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () =>
            Promise.resolve({
              id: 42,
              email: "updated@test.com",
              name: "Updated",
              role: "admin",
              created_at: "",
              updated_at: "",
            }),
        });
      }

      if (url.endsWith("/api/auth/profile")) {
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () =>
            Promise.resolve({
              id: 1,
              email: "me@test.com",
              name: "Me",
              role: "admin",
              created_at: "",
              updated_at: "",
            }),
        });
      }

      if (url.endsWith("/api/auth/avatar")) {
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () => Promise.resolve({ avatar_url: "/avatars/me.png" }),
        });
      }

      return Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve({}) });
    }) as unknown as typeof fetch;

    await createExperiment({ experiment_name: "exp", variant: "control", prompt_template: "prompt" });
    await updateExperiment(1, { experiment_name: "exp", variant: "treatment", prompt_template: "prompt" });
    await fetchConfig();
    await saveConfig("new config");
    await fetchRepoLearning("org/repo name");
    await getUser(42);
    await updateUser(42, { name: "Updated", role: "admin" });
    await updateProfile({ name: "Me" });
    await uploadAvatar(new File(["avatar"], "avatar.png", { type: "image/png" }));

    const postCall = calls.find((call) => call.url.endsWith("/api/experiments"));
    const experimentUpdateCall = calls.find((call) => call.url.endsWith("/api/experiments/1"));
    const configPutCall = calls.find((call) => call.url.endsWith("/api/config") && call.init?.method === "PUT");
    const repoLearningCall = calls.find((call) => call.url.includes("/api/repos/") && call.url.endsWith("/learning"));
    const userUpdateCall = calls.find((call) => call.url.endsWith("/api/users/42") && call.init?.method === "PUT");
    const profilePutCall = calls.find((call) => call.url.endsWith("/api/auth/profile"));
    const avatarCall = calls.find((call) => call.url.endsWith("/api/auth/avatar"));

    expect(postCall).toBeTruthy();
    expect(postCall?.init?.method).toBe("POST");
    expect(postCall?.init?.headers).toEqual({ "Content-Type": "application/json", "x-csrf-token": "" });
    expect(postCall?.init?.body).toBe(
      JSON.stringify({ experiment_name: "exp", variant: "control", prompt_template: "prompt" })
    );

    expect(experimentUpdateCall?.init?.method).toBe("PUT");
    expect(String(configPutCall?.init?.body)).toBe(JSON.stringify({ content: "new config" }));
    expect(repoLearningCall?.url).toContain("/api/repos/org%2Frepo%20name/learning");
    expect(userUpdateCall?.init?.method).toBe("PUT");
    expect(profilePutCall?.init?.method).toBe("PUT");
    expect(avatarCall?.init?.method).toBe("POST");
    expect(avatarCall?.init?.body instanceof FormData).toBe(true);
  });

  test("post, put, delete, and upload helpers propagate unauthorized and invoke callback", async () => {
    let unauthorizedCount = 0;
    setOnUnauthorized(() => {
      unauthorizedCount += 1;
    });

    mockFetchResponse({ status: 401, data: { error: "unauthorized" } });

    await expect(createExperiment({ experiment_name: "x", variant: "y", prompt_template: "z" })).rejects.toThrow(
      "Unauthorized"
    );
    await expect(updateExperiment(1, { experiment_name: "x", variant: "y", prompt_template: "z" })).rejects.toThrow(
      "Unauthorized"
    );
    await expect(deleteUser(1)).rejects.toThrow("Unauthorized");
    await expect(uploadAvatar(new File(["x"], "a.png", { type: "image/png" }))).rejects.toThrow("Unauthorized");

    expect(unauthorizedCount).toBe(4);
  });

  test("post/put/delete/upload helpers throw on non-401 error responses", async () => {
    mockFetchResponse({ status: 500, data: { error: "server" } });

    await expect(createExperiment({ experiment_name: "x", variant: "y", prompt_template: "z" })).rejects.toThrow(
      "Failed to post /api/experiments: 500"
    );
    await expect(saveConfig("abc")).rejects.toThrow("Failed to put /api/config: 500");
    await expect(deleteUser(1)).rejects.toThrow("Failed to delete /api/users/1: 500");
    await expect(uploadAvatar(new File(["x"], "a.png", { type: "image/png" }))).rejects.toThrow(
      "Failed to upload avatar: 500"
    );
  });
});
