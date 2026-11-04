import { afterEach, describe, expect, mock, test } from "bun:test";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SWRConfig } from "swr";
import type { ReactNode } from "react";
import { AuthProvider } from "../src/lib/auth";
import { PASSWORD_LENGTH_MESSAGE, passwordByteLength, passwordLengthProblem } from "../src/lib/password";
import SettingsPage from "../src/pages/settings";

const GRINNING_FACE = "\u{1F600}";

describe("password length", () => {
  test("counts UTF-8 bytes, not UTF-16 code units", () => {
    expect(passwordByteLength("abc")).toBe(3);
    expect(passwordByteLength("é")).toBe(2);
    expect(passwordByteLength(GRINNING_FACE)).toBe(4);
    expect(GRINNING_FACE.length).toBe(2);
  });

  test("accepts 8-72 bytes and names bytes in the message", () => {
    expect(PASSWORD_LENGTH_MESSAGE).toBe("Password must be 8-72 bytes");
    expect(passwordLengthProblem("x".repeat(7))).toBe(PASSWORD_LENGTH_MESSAGE);
    expect(passwordLengthProblem("x".repeat(8))).toBeNull();
    expect(passwordLengthProblem("x".repeat(72))).toBeNull();
    expect(passwordLengthProblem("x".repeat(73))).toBe(PASSWORD_LENGTH_MESSAGE);
    expect(passwordLengthProblem(GRINNING_FACE.repeat(18))).toBeNull();
    expect(passwordLengthProblem(GRINNING_FACE.repeat(19))).toBe(PASSWORD_LENGTH_MESSAGE);
    expect(passwordLengthProblem(GRINNING_FACE.repeat(2))).toBeNull();
  });
});

describe("Settings password change", () => {
  const originalFetch = globalThis.fetch;

  afterEach(() => {
    cleanup();
    globalThis.fetch = originalFetch;
  });

  function Wrapper({ children }: { children: ReactNode }) {
    return (
      <SWRConfig value={{ provider: () => new Map(), dedupingInterval: 0 }}>
        <AuthProvider>{children}</AuthProvider>
      </SWRConfig>
    );
  }

  test("rejects a new password over 72 UTF-8 bytes without calling the server", async () => {
    const methods: string[] = [];
    globalThis.fetch = mock((input: string | URL | Request, init?: RequestInit) => {
      const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
      methods.push(`${init?.method ?? "GET"} ${new URL(url, "http://localhost").pathname}`);
      if (!url.endsWith("/api/auth/me")) {
        return Promise.resolve({ ok: false, status: 404, json: () => Promise.resolve({ error: "Not found" }) });
      }
      const me = { id: 3, email: "jake@example.com", name: "Jake", role: "viewer", avatar_url: null };
      return Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve(me) });
    }) as unknown as typeof fetch;

    render(<SettingsPage />, { wrapper: Wrapper });
    await waitFor(() => expect(screen.getByLabelText("New Password")).toBeTruthy());

    const tooLong = GRINNING_FACE.repeat(30);
    fireEvent.change(screen.getByLabelText("Current Password"), { target: { value: "current password" } });
    fireEvent.change(screen.getByLabelText("New Password"), { target: { value: tooLong } });
    fireEvent.change(screen.getByLabelText("Confirm New Password"), { target: { value: tooLong } });
    fireEvent.click(screen.getByRole("button", { name: "Change Password" }));

    expect(await screen.findByText(PASSWORD_LENGTH_MESSAGE)).toBeTruthy();
    expect(methods).not.toContain("PUT /api/auth/profile");
  });
});
