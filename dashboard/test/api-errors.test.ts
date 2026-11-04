import { afterEach, describe, expect, mock, test } from "bun:test";
import {
  ApiError,
  createExperiment,
  deleteUser,
  fetchConfig,
  saveConfig,
  setOnUnauthorized,
} from "../src/lib/api";

type MockBody = { kind: "json"; data: unknown } | { kind: "invalid" } | { kind: "missing" };

function mockFetch(status: number, body: MockBody) {
  globalThis.fetch = mock(() =>
    Promise.resolve({
      ok: status >= 200 && status < 300,
      status,
      statusText: "Error",
      headers: new Headers({ "content-type": "application/json" }),
      ...(body.kind === "json"
        ? { json: () => Promise.resolve(body.data) }
        : body.kind === "invalid"
          ? { json: () => Promise.reject(new SyntaxError("Unexpected token <")) }
          : {}),
    })
  ) as unknown as typeof fetch;
}

async function captureError(promise: Promise<unknown>): Promise<unknown> {
  try {
    await promise;
  } catch (error) {
    return error;
  }
  throw new Error("expected the request to reject");
}

describe("ApiError", () => {
  const originalFetch = globalThis.fetch;

  afterEach(() => {
    globalThis.fetch = originalFetch;
    setOnUnauthorized(() => {});
  });

  test("is an Error carrying status, message and optional code", () => {
    const error = new ApiError(409, "Config changed on disk", "revision_mismatch");
    expect(error).toBeInstanceOf(Error);
    expect(error.name).toBe("ApiError");
    expect(error.status).toBe(409);
    expect(error.message).toBe("Config changed on disk");
    expect(error.code).toBe("revision_mismatch");
    expect(new ApiError(500, "boom").code).toBeUndefined();
  });

  test("surfaces the server error message and code from a PUT", async () => {
    mockFetch(400, { kind: "json", data: { error: "Config validation failed: X", code: "invalid" } });

    const error = await captureError(saveConfig("bad"));

    expect(error).toBeInstanceOf(ApiError);
    const apiError = error as ApiError;
    expect(apiError.message).toBe("Config validation failed: X");
    expect(apiError.status).toBe(400);
    expect(apiError.code).toBe("invalid");
  });

  test("surfaces the server error message from GET, POST and DELETE", async () => {
    mockFetch(403, { kind: "json", data: { error: "Admin access required" } });

    for (const request of [
      () => fetchConfig(),
      () => createExperiment({ experiment_name: "x", variant: "y", prompt_template: "z" }),
      () => deleteUser(1),
    ]) {
      const error = await captureError(request());
      expect(error).toBeInstanceOf(ApiError);
      expect((error as ApiError).message).toBe("Admin access required");
      expect((error as ApiError).status).toBe(403);
      expect((error as ApiError).code).toBeUndefined();
    }
  });

  test("falls back to the generic text when the body has no string error", async () => {
    mockFetch(500, { kind: "json", data: { error: { nested: true }, code: 7 } });
    const nonString = (await captureError(saveConfig("abc"))) as ApiError;
    expect(nonString.message).toBe("Failed to put /api/config: 500");
    expect(nonString.code).toBeUndefined();

    mockFetch(502, { kind: "invalid" });
    const invalidJson = (await captureError(fetchConfig())) as ApiError;
    expect(invalidJson).toBeInstanceOf(ApiError);
    expect(invalidJson.message).toBe("Failed to fetch /api/config: 502");
    expect(invalidJson.status).toBe(502);

    mockFetch(500, { kind: "missing" });
    const noBody = (await captureError(deleteUser(2))) as ApiError;
    expect(noBody.message).toBe("Failed to delete /api/users/2: 500");

    mockFetch(500, { kind: "json", data: null });
    const nullBody = (await captureError(createExperiment({ experiment_name: "x", variant: "y", prompt_template: "z" }))) as ApiError;
    expect(nullBody.message).toBe("Failed to post /api/experiments: 500");
  });

  test("keeps the unauthorized behaviour", async () => {
    let unauthorizedCount = 0;
    setOnUnauthorized(() => {
      unauthorizedCount += 1;
    });
    mockFetch(401, { kind: "json", data: { error: "session expired" } });

    const error = await captureError(saveConfig("abc"));

    expect(error).not.toBeInstanceOf(ApiError);
    expect((error as Error).message).toBe("Unauthorized");
    expect(unauthorizedCount).toBe(1);
  });
});
