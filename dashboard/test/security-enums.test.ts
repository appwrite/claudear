import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import {
  AWARD_STATUSES,
  DELIVERY_STATUSES,
  SECURITY_EVENT_KINDS,
  SECURITY_OUTCOMES,
  SECURITY_REPLY_KINDS,
  SECURITY_REPORT_STATES,
  SECURITY_SEVERITIES,
  VULNERABILITY_STATES,
} from "../src/lib/api";

const dashboardValues: Record<string, readonly string[]> = {
  SecurityReportState: SECURITY_REPORT_STATES,
  VulnerabilityState: VULNERABILITY_STATES,
  SecurityOutcome: SECURITY_OUTCOMES,
  SecurityReplyKind: SECURITY_REPLY_KINDS,
  SecuritySeverity: SECURITY_SEVERITIES,
  DeliveryStatus: DELIVERY_STATUSES,
  AwardStatus: AWARD_STATUSES,
  SecurityEventKind: SECURITY_EVENT_KINDS,
};

function serverEnums(): Map<string, string[]> {
  const source = readFileSync(new URL("../../crates/claudear-core/src/security.rs", import.meta.url), "utf8");
  const enums = new Map<string, string[]>();
  for (const [, name, body] of source.matchAll(/string_enum!\s*\{\s*(\w+)\s*\{([^}]*)\}/g)) {
    enums.set(
      name,
      [...body.matchAll(/=>\s*"([^"]+)"/g)].map(([, value]) => value),
    );
  }
  return enums;
}

describe("security enums", () => {
  test("every server security enum has a dashboard list", () => {
    const enums = serverEnums();
    expect(enums.size).toBeGreaterThan(0);
    expect([...enums.keys()].sort()).toEqual(Object.keys(dashboardValues).sort());
  });

  test("the dashboard knows every value the server can send", () => {
    for (const [name, values] of serverEnums()) {
      const known = new Set(dashboardValues[name]);
      expect({ name, missing: values.filter((value) => !known.has(value)) }).toEqual({ name, missing: [] });
    }
  });
});
