import { describe, expect, test } from "bun:test";

describe("test isolation", () => {
  test("a test sets cookies with and without a path", () => {
    document.cookie = "claudear_csrf=csrf-token";
    document.cookie = "claudear_theme=dark; path=/";
    expect(document.cookie).toContain("claudear_csrf=csrf-token");
  });

  test("the next test starts with no cookies", () => {
    expect(document.cookie).toBe("");
  });
});
