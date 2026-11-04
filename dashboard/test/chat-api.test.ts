import { describe, expect, test, mock, afterEach } from "bun:test";
import {
  fetchChatSessions,
  fetchChatSession,
  deleteChatSession,
  fetchChatModels,
  isChatDisabled,
} from "../src/lib/chat";
import { ApiError } from "../src/lib/api";

function mockFetch(data: unknown, ok = true, status = ok ? 200 : 500) {
  globalThis.fetch = mock(() =>
    Promise.resolve({
      ok,
      status,
      statusText: ok ? "OK" : "Internal Server Error",
      json: () => Promise.resolve(data),
    })
  ) as unknown as typeof fetch;
}

describe("chat REST API helpers", () => {
  const originalFetch = globalThis.fetch;

  afterEach(() => {
    globalThis.fetch = originalFetch;
  });

  describe("fetchChatSessions", () => {
    test("calls correct URL and returns sessions", async () => {
      const sessions = [
        { id: "s1", created_at: "2024-01-01T00:00:00Z", updated_at: "2024-01-01T01:00:00Z" },
      ];
      mockFetch(sessions);

      const result = await fetchChatSessions();
      expect(result).toEqual(sessions);
      expect(fetch).toHaveBeenCalledWith("/api/chat/sessions");
    });

    test("throws on error response", async () => {
      mockFetch(null, false);
      expect(fetchChatSessions()).rejects.toThrow("Failed to fetch sessions");
    });
  });

  describe("fetchChatSession", () => {
    test("calls correct URL with session id", async () => {
      const session = {
        id: "sess-123",
        created_at: "2024-01-01T00:00:00Z",
        updated_at: "2024-01-01T01:00:00Z",
        messages: [],
      };
      mockFetch(session);

      const result = await fetchChatSession("sess-123");
      expect(result).toEqual(session);
      expect(fetch).toHaveBeenCalledWith("/api/chat/sessions/sess-123");
    });

    test("throws on error response", async () => {
      mockFetch(null, false);
      expect(fetchChatSession("bad-id")).rejects.toThrow("Failed to fetch session");
    });
  });

  describe("deleteChatSession", () => {
    test("calls correct URL with DELETE method", async () => {
      mockFetch(null);

      document.cookie = "claudear_csrf=csrf-token";
      await deleteChatSession("sess-456");
      expect(fetch).toHaveBeenCalledWith("/api/chat/sessions/sess-456", {
        method: "DELETE",
        headers: { "x-csrf-token": "csrf-token" },
      });
    });

    test("throws on error response", async () => {
      mockFetch(null, false);
      expect(deleteChatSession("bad-id")).rejects.toThrow("Failed to delete session");
    });
  });

  describe("fetchChatModels", () => {
    test("calls correct URL and returns models", async () => {
      const data = {
        models: [
          { name: "phi-3-mini", status: "ready", context_length: 4096 },
        ],
      };
      mockFetch(data);

      const result = await fetchChatModels();
      expect(result).toEqual(data);
      expect(fetch).toHaveBeenCalledWith("/api/chat/models");
    });

    test("throws on error response", async () => {
      mockFetch(null, false);
      expect(fetchChatModels()).rejects.toThrow("Failed to fetch models");
    });
  });

  describe("disabled chat", () => {
    test("a 404 rejects with an ApiError carrying the status", async () => {
      mockFetch(null, false, 404);
      const error = await fetchChatSessions().catch((caught: unknown) => caught);
      expect(error).toBeInstanceOf(ApiError);
      expect((error as ApiError).status).toBe(404);
      expect(isChatDisabled(error)).toBe(true);
    });

    test("other failures do not read as disabled", async () => {
      mockFetch(null, false, 500);
      const error = await fetchChatModels().catch((caught: unknown) => caught);
      expect((error as ApiError).status).toBe(500);
      expect(isChatDisabled(error)).toBe(false);
      expect(isChatDisabled(new Error("Not Found"))).toBe(false);
    });
  });
});
