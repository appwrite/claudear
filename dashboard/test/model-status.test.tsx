import { afterEach, describe, expect, mock, test } from "bun:test";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { ModelStatus } from "../src/components/chat/ModelStatus";
import { SWRConfig } from "swr";

afterEach(() => {
  cleanup();
});

function mockFetch(data: unknown, ok = true) {
  globalThis.fetch = mock((url: string) =>
    Promise.resolve({
      ok,
      status: ok ? 200 : 500,
      json: () => Promise.resolve(url.endsWith("/api/chat/status") ? { enabled: true } : data),
    })
  ) as unknown as typeof fetch;
}

// Wrap with SWRConfig to disable cache between tests and dedupe
function renderModelStatus() {
  return render(
    <SWRConfig value={{ provider: () => new Map(), dedupingInterval: 0 }}>
      <ModelStatus />
    </SWRConfig>
  );
}

describe("ModelStatus", () => {
  const originalFetch = globalThis.fetch;

  afterEach(() => {
    globalThis.fetch = originalFetch;
  });

  test("shows 'No model' when fetch fails", async () => {
    mockFetch(null, false);
    renderModelStatus();
    await waitFor(() => {
      expect(screen.getByText("No model")).toBeTruthy();
    });
  });

  test("shows 'No model' when models array is empty", async () => {
    mockFetch({ models: [] });
    renderModelStatus();
    await waitFor(() => {
      expect(screen.getByText("No model")).toBeTruthy();
    });
  });

  test("shows model name when status is ready", async () => {
    mockFetch({
      models: [{ name: "phi-3-mini", status: "ready", context_length: 4096 }],
    });
    renderModelStatus();
    await waitFor(() => {
      expect(screen.getByText("phi-3-mini")).toBeTruthy();
    });
  });

  test("shows model name when status is loading", async () => {
    mockFetch({
      models: [{ name: "llama-2", status: "loading", context_length: 8192 }],
    });
    renderModelStatus();
    await waitFor(() => {
      expect(screen.getByText("llama-2")).toBeTruthy();
    });
  });

  test("shows model name when status is notloaded", async () => {
    mockFetch({
      models: [{ name: "codellama", status: "notloaded", context_length: 4096 }],
    });
    renderModelStatus();
    await waitFor(() => {
      expect(screen.getByText("codellama")).toBeTruthy();
    });
  });

  test("shows model name when status is error", async () => {
    mockFetch({
      models: [{ name: "broken-model", status: "error", context_length: 0 }],
    });
    renderModelStatus();
    await waitFor(() => {
      expect(screen.getByText("broken-model")).toBeTruthy();
    });
  });

  test("model name is in a span with title for tooltip", async () => {
    mockFetch({
      models: [{ name: "my-model-name", status: "ready", context_length: 4096 }],
    });
    renderModelStatus();
    await waitFor(() => {
      const el = screen.getByTitle("my-model-name");
      expect(el).toBeTruthy();
      expect(el.textContent).toBe("my-model-name");
    });
  });
});
