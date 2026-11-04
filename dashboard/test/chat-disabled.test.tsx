import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { SWRConfig } from "swr";
import type { ReactNode } from "react";
import { AuthProvider } from "../src/lib/auth";
import ChatPage from "../src/pages/chat";
import ModelsPage from "../src/pages/models";
import { ModelStatus } from "../src/components/chat/ModelStatus";
import { CHAT_DISABLED_MESSAGE } from "../src/components/chat/ChatDisabledNotice";

type Role = "admin" | "viewer";

const OriginalWebSocket = globalThis.WebSocket;
const originalFetch = globalThis.fetch;
let constructedSockets = 0;

class CountingWebSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  readyState = CountingWebSocket.CONNECTING;
  onopen: ((ev: Event) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;

  constructor() {
    constructedSockets += 1;
  }

  send() {}

  close() {
    this.readyState = CountingWebSocket.CLOSED;
  }
}

function json(status: number, body: unknown) {
  return Promise.resolve({
    ok: status >= 200 && status < 300,
    status,
    statusText: status === 404 ? "Not Found" : status >= 400 ? "Error" : "OK",
    json: () => Promise.resolve(body),
  });
}

type Availability = { status: 200; enabled: boolean } | { status: 404 | 500 };

const MOUNTED: Availability = { status: 200, enabled: true };

function mockServer(role: Role, chatStatus: number, availability: Availability = MOUNTED): string[] {
  const requested: string[] = [];
  globalThis.fetch = mock((input: string | URL | Request) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
    requested.push(url);
    if (url.endsWith("/api/auth/me")) {
      return json(200, { id: 1, email: `${role}@test.com`, name: role, role });
    }
    if (url.endsWith("/api/chat/status")) {
      return availability.status === 200
        ? json(200, { enabled: availability.enabled })
        : json(availability.status, { error: "status failed" });
    }
    if (url.endsWith("/api/chat/sessions")) {
      return json(chatStatus, chatStatus === 200 ? [] : { error: chatStatus === 404 ? "not found" : "chat failed" });
    }
    if (url.endsWith("/api/chat/models")) {
      return json(
        chatStatus,
        chatStatus === 200
          ? { models: [{ name: "qwen-coder", status: "ready", context_length: 4096 }] }
          : { error: "not found" },
      );
    }
    return json(200, {});
  }) as unknown as typeof fetch;
  return requested;
}

function Wrapper({ children }: { children: ReactNode }) {
  return (
    <SWRConfig value={{ provider: () => new Map(), dedupingInterval: 0 }}>
      <AuthProvider>{children}</AuthProvider>
    </SWRConfig>
  );
}

function chatProbes(requested: string[]): string[] {
  return requested.filter((url) => url.includes("/api/chat/") && !url.endsWith("/api/chat/status"));
}

async function settle() {
  await new Promise((resolve) => setTimeout(resolve, 50));
}

describe("chat disabled state", () => {
  beforeEach(() => {
    constructedSockets = 0;
    globalThis.WebSocket = CountingWebSocket as unknown as typeof WebSocket;
  });

  afterEach(() => {
    cleanup();
    globalThis.fetch = originalFetch;
    globalThis.WebSocket = OriginalWebSocket;
  });

  test("chat page shows the notice and never opens a websocket on 404", async () => {
    mockServer("admin", 404);
    render(<ChatPage />, { wrapper: Wrapper });

    await waitFor(() => {
      expect(screen.getByText(CHAT_DISABLED_MESSAGE)).toBeTruthy();
    });
    await waitFor(() => {
      expect(screen.getByRole("button", { name: "Open Config" })).toBeTruthy();
    });
    expect(screen.getByText("Chat disabled")).toBeTruthy();
    expect(screen.queryByText("Disconnected")).toBeNull();
    expect(screen.queryByPlaceholderText("Connecting...")).toBeNull();
    expect(constructedSockets).toBe(0);
  });

  test("viewers see the notice without the config link", async () => {
    const requested = mockServer("viewer", 404);
    render(<ChatPage />, { wrapper: Wrapper });

    await waitFor(() => {
      expect(screen.getByText(CHAT_DISABLED_MESSAGE)).toBeTruthy();
    });
    expect(requested).toContain("/api/auth/me");
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(screen.queryByRole("button", { name: "Open Config" })).toBeNull();
    expect(constructedSockets).toBe(0);
  });

  test("chat page opens the websocket once sessions load", async () => {
    mockServer("admin", 200);
    render(<ChatPage />, { wrapper: Wrapper });

    await waitFor(() => {
      expect(constructedSockets).toBe(1);
    });
    expect(screen.queryByText(CHAT_DISABLED_MESSAGE)).toBeNull();
  });

  for (const status of [401, 500]) {
    test(`chat page shows an error and never opens a websocket on ${status}`, async () => {
      mockServer("admin", status);
      render(<ChatPage />, { wrapper: Wrapper });

      await waitFor(() => {
        expect(screen.getByText("Failed to load chat sessions.")).toBeTruthy();
      });
      await new Promise((resolve) => setTimeout(resolve, 50));
      expect(screen.queryByText(CHAT_DISABLED_MESSAGE)).toBeNull();
      expect(screen.queryByText("Disconnected")).toBeNull();
      expect(screen.queryByPlaceholderText("Connecting...")).toBeNull();
      expect(constructedSockets).toBe(0);
    });
  }

  test("models page shows the notice and keeps HuggingFace browsing", async () => {
    mockServer("admin", 404);
    render(<ModelsPage />, { wrapper: Wrapper });

    await waitFor(() => {
      expect(screen.getByText(CHAT_DISABLED_MESSAGE)).toBeTruthy();
    });
    expect(screen.queryByText("No model configured")).toBeNull();
    expect(screen.getByText("Browse HuggingFace GGUF Models")).toBeTruthy();
    expect(screen.getByPlaceholderText("Search models (e.g., qwen, llama, codestral)...")).toBeTruthy();
  });

  test("model status shows chat disabled on 404", async () => {
    mockServer("viewer", 404);
    render(<ModelStatus />, { wrapper: Wrapper });

    await waitFor(() => {
      expect(screen.getByText("Chat disabled")).toBeTruthy();
    });
    expect(screen.queryByText("No model")).toBeNull();
  });
  for (const availability of [{ status: 200, enabled: false }, { status: 404 }] as const) {
    const label = availability.status === 200 ? "reports chat unmounted" : "is missing";
    test(`chat page probes no chat route and opens no websocket when the status ${label}`, async () => {
      const requested = mockServer("admin", 404, availability);
      render(<ChatPage />, { wrapper: Wrapper });

      await waitFor(() => {
        expect(screen.getByText(CHAT_DISABLED_MESSAGE)).toBeTruthy();
      });
      expect(screen.getByText("Chat disabled")).toBeTruthy();
      await settle();
      expect(requested).toContain("/api/chat/status");
      expect(chatProbes(requested)).toEqual([]);
      expect(constructedSockets).toBe(0);
    });

    test(`models page probes no chat route when the status ${label}`, async () => {
      const requested = mockServer("admin", 404, availability);
      render(<ModelsPage />, { wrapper: Wrapper });

      await waitFor(() => {
        expect(screen.getByText(CHAT_DISABLED_MESSAGE)).toBeTruthy();
      });
      expect(screen.getByText("Browse HuggingFace GGUF Models")).toBeTruthy();
      await settle();
      expect(chatProbes(requested)).toEqual([]);
      expect(constructedSockets).toBe(0);
    });

    test(`model status probes no chat route when the status ${label}`, async () => {
      const requested = mockServer("viewer", 404, availability);
      render(<ModelStatus />, { wrapper: Wrapper });

      await waitFor(() => {
        expect(screen.getByText("Chat disabled")).toBeTruthy();
      });
      await settle();
      expect(chatProbes(requested)).toEqual([]);
    });
  }

  test("chat page shows an error and probes no chat route when the status fails", async () => {
    const requested = mockServer("admin", 200, { status: 500 });
    render(<ChatPage />, { wrapper: Wrapper });

    await waitFor(() => {
      expect(screen.getByText("Failed to load chat sessions.")).toBeTruthy();
    });
    await settle();
    expect(screen.queryByText(CHAT_DISABLED_MESSAGE)).toBeNull();
    expect(chatProbes(requested)).toEqual([]);
    expect(constructedSockets).toBe(0);
  });

  test("chat page loads sessions and models once the status reports chat mounted", async () => {
    const requested = mockServer("admin", 200);
    render(<ChatPage />, { wrapper: Wrapper });

    await waitFor(() => {
      expect(constructedSockets).toBe(1);
    });
    expect(requested).toContain("/api/chat/sessions");
    expect(requested).toContain("/api/chat/models");
    expect(screen.getByText("qwen-coder")).toBeTruthy();
  });
});
