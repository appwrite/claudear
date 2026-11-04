import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, render, screen, act, fireEvent } from "@testing-library/react";
import { useChat } from "../src/lib/chat";

afterEach(() => {
  cleanup();
});

// Capture the most recent WebSocket instance created during a test.
let lastWs: InstanceType<typeof WebSocket> | null = null;

const OriginalWebSocket = globalThis.WebSocket;

class MockWebSocket extends EventTarget {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  readonly CONNECTING = 0;
  readonly OPEN = 1;
  readonly CLOSING = 2;
  readonly CLOSED = 3;
  readyState = MockWebSocket.CONNECTING;
  url: string;
  onopen: ((ev: Event) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  sentMessages: string[] = [];

  constructor(url: string | URL) {
    super();
    this.url = String(url);
    lastWs = this as unknown as InstanceType<typeof WebSocket>;
  }

  send(data: string) {
    this.sentMessages.push(data);
  }

  close() {
    this.readyState = MockWebSocket.CLOSED;
    this.onclose?.(new CloseEvent("close"));
  }

  // Test helpers
  simulateOpen() {
    this.readyState = MockWebSocket.OPEN;
    this.onopen?.(new Event("open"));
  }

  simulateMessage(data: unknown) {
    this.onmessage?.(new MessageEvent("message", { data: JSON.stringify(data) }));
  }

  simulateClose() {
    this.readyState = MockWebSocket.CLOSED;
    this.onclose?.(new CloseEvent("close"));
  }
}

// Helper component to consume the hook and expose its state.
function ChatHarness({ repoId, enabled }: { repoId?: number; enabled?: boolean }) {
  const { messages, sendMessage, stopStreaming, isStreaming, isConnected, error, sessionId, clearMessages } =
    useChat(repoId, enabled);

  return (
    <div>
      <span data-testid="connected">{String(isConnected)}</span>
      <span data-testid="streaming">{String(isStreaming)}</span>
      <span data-testid="error">{error ?? ""}</span>
      <span data-testid="session">{sessionId ?? ""}</span>
      <span data-testid="count">{messages.length}</span>
      {messages.map((m, i) => (
        <div key={i} data-testid={`msg-${i}`}>
          <span data-testid={`msg-${i}-role`}>{m.role}</span>
          <span data-testid={`msg-${i}-content`}>{m.content}</span>
          <span data-testid={`msg-${i}-sources`}>{m.sources?.length ?? 0}</span>
        </div>
      ))}
      <button data-testid="send" onClick={() => sendMessage("test message")}>Send</button>
      <button data-testid="stop" onClick={stopStreaming}>Stop</button>
      <button data-testid="clear" onClick={clearMessages}>Clear</button>
    </div>
  );
}

function getWs(): MockWebSocket {
  return lastWs as unknown as MockWebSocket;
}

describe("useChat hook", () => {
  afterEach(() => {
    lastWs = null;
    globalThis.WebSocket = OriginalWebSocket;
  });

  function setup(repoId?: number) {
    globalThis.WebSocket = MockWebSocket as unknown as typeof WebSocket;
    const result = render(<ChatHarness repoId={repoId} />);
    return result;
  }

  test("does not open a websocket when disabled", () => {
    globalThis.WebSocket = MockWebSocket as unknown as typeof WebSocket;
    render(<ChatHarness enabled={false} />);
    expect(lastWs).toBeNull();
    expect(screen.getByTestId("connected").textContent).toBe("false");
    fireEvent.click(screen.getByTestId("send"));
    expect(screen.getByTestId("error").textContent).toBe("Not connected");
  });

  test("opens the websocket once enabled", () => {
    globalThis.WebSocket = MockWebSocket as unknown as typeof WebSocket;
    const { rerender } = render(<ChatHarness enabled={false} />);
    expect(lastWs).toBeNull();
    rerender(<ChatHarness enabled />);
    expect(lastWs).not.toBeNull();
    act(() => getWs().simulateOpen());
    expect(screen.getByTestId("connected").textContent).toBe("true");
  });

  test("starts disconnected", () => {
    setup();
    expect(screen.getByTestId("connected").textContent).toBe("false");
    expect(screen.getByTestId("streaming").textContent).toBe("false");
  });

  test("becomes connected on ws open", () => {
    setup();
    act(() => getWs().simulateOpen());
    expect(screen.getByTestId("connected").textContent).toBe("true");
  });

  test("becomes disconnected on ws close", () => {
    setup();
    act(() => getWs().simulateOpen());
    expect(screen.getByTestId("connected").textContent).toBe("true");

    act(() => getWs().simulateClose());
    expect(screen.getByTestId("connected").textContent).toBe("false");
  });

  test("sendMessage adds user and assistant messages", () => {
    setup();
    act(() => getWs().simulateOpen());

    act(() => {
      fireEvent.click(screen.getByTestId("send"));
    });

    expect(screen.getByTestId("count").textContent).toBe("2");
    expect(screen.getByTestId("msg-0-role").textContent).toBe("user");
    expect(screen.getByTestId("msg-0-content").textContent).toBe("test message");
    expect(screen.getByTestId("msg-1-role").textContent).toBe("assistant");
    expect(screen.getByTestId("msg-1-content").textContent).toBe("");
  });

  test("sendMessage sends JSON with type:chat over WebSocket", () => {
    setup(42);
    act(() => getWs().simulateOpen());

    act(() => {
      fireEvent.click(screen.getByTestId("send"));
    });

    const ws = getWs();
    expect(ws.sentMessages.length).toBe(1);
    const parsed = JSON.parse(ws.sentMessages[0]);
    expect(parsed.type).toBe("chat");
    expect(parsed.message).toBe("test message");
    expect(parsed.repo_id).toBe(42);
  });

  test("sendMessage sets isStreaming to true", () => {
    setup();
    act(() => getWs().simulateOpen());

    act(() => {
      fireEvent.click(screen.getByTestId("send"));
    });

    expect(screen.getByTestId("streaming").textContent).toBe("true");
  });

  test("sendMessage sets error when not connected", () => {
    setup();
    // Don't open the WebSocket

    act(() => {
      fireEvent.click(screen.getByTestId("send"));
    });

    expect(screen.getByTestId("error").textContent).toBe("Not connected");
  });

  test("streaming chunk updates assistant message content", () => {
    setup();
    act(() => getWs().simulateOpen());

    act(() => {
      fireEvent.click(screen.getByTestId("send"));
    });

    act(() => {
      getWs().simulateMessage({ delta: "Hello", done: false, session_id: "s1" });
    });

    expect(screen.getByTestId("msg-1-content").textContent).toBe("Hello");
    expect(screen.getByTestId("session").textContent).toBe("s1");
  });

  test("multiple chunks accumulate content", () => {
    setup();
    act(() => getWs().simulateOpen());

    act(() => {
      fireEvent.click(screen.getByTestId("send"));
    });

    act(() => {
      getWs().simulateMessage({ delta: "Hello", done: false, session_id: "s1" });
    });
    act(() => {
      getWs().simulateMessage({ delta: " world", done: false });
    });

    expect(screen.getByTestId("msg-1-content").textContent).toBe("Hello world");
  });

  test("done chunk sets isStreaming false and attaches sources", () => {
    setup();
    act(() => getWs().simulateOpen());

    act(() => {
      fireEvent.click(screen.getByTestId("send"));
    });

    act(() => {
      getWs().simulateMessage({ delta: "Hi", done: false, session_id: "s1" });
    });
    act(() => {
      getWs().simulateMessage({
        delta: "",
        done: true,
        sources: [{ file_path: "a.rs", start_line: 1, end_line: 5, similarity: 0.9 }],
      });
    });

    expect(screen.getByTestId("streaming").textContent).toBe("false");
    expect(screen.getByTestId("msg-1-sources").textContent).toBe("1");
  });

  test("error chunk sets error and stops streaming", () => {
    setup();
    act(() => getWs().simulateOpen());

    act(() => {
      fireEvent.click(screen.getByTestId("send"));
    });

    act(() => {
      getWs().simulateMessage({ delta: "", done: false, error: "model not found" });
    });

    expect(screen.getByTestId("error").textContent).toBe("model not found");
    expect(screen.getByTestId("streaming").textContent).toBe("false");
  });

  test("stopStreaming sends stop message", () => {
    setup();
    act(() => getWs().simulateOpen());

    act(() => {
      fireEvent.click(screen.getByTestId("send"));
    });

    act(() => {
      fireEvent.click(screen.getByTestId("stop"));
    });

    const ws = getWs();
    expect(ws.sentMessages.length).toBe(2);
    const stopMsg = JSON.parse(ws.sentMessages[1]);
    expect(stopMsg.type).toBe("stop");
  });

  test("clearMessages resets state", () => {
    setup();
    act(() => getWs().simulateOpen());

    act(() => {
      fireEvent.click(screen.getByTestId("send"));
    });
    act(() => {
      getWs().simulateMessage({ delta: "Hi", done: true, session_id: "s1", sources: [] });
    });

    expect(screen.getByTestId("count").textContent).toBe("2");
    expect(screen.getByTestId("session").textContent).toBe("s1");

    act(() => {
      fireEvent.click(screen.getByTestId("clear"));
    });

    expect(screen.getByTestId("count").textContent).toBe("0");
    expect(screen.getByTestId("session").textContent).toBe("");
  });

  test("ignores malformed WebSocket messages", () => {
    setup();
    act(() => getWs().simulateOpen());

    act(() => {
      fireEvent.click(screen.getByTestId("send"));
    });

    // Send a raw non-JSON message
    act(() => {
      const ws = getWs();
      ws.onmessage?.(new MessageEvent("message", { data: "not json" }));
    });

    // Should not crash - streaming still active, no error set from malformed msg
    expect(screen.getByTestId("streaming").textContent).toBe("true");
  });
});
