import { describe, expect, test } from "bun:test";

import { runSessionHarness } from "../src/harness.ts";

describe("session lifecycle harness", () => {
  test(
    "survives concurrent chats, reconnects, handoff, conflicts and cleanup",
    async () => {
      const report = await runSessionHarness();
      expect(report).toMatchObject({
        chats: 20,
        initial_leases: 20,
        handoff_leases: 20,
        final_leases: 0,
        ttl_verified: true,
        reconnects_tested: 1,
        recovered_operation: true,
        cleanup: "clean",
      });
      expect(report.events).toBeGreaterThan(60);
      expect(report.heartbeats).toBeGreaterThanOrEqual(20);
      expect(report.selector_checks).toBeGreaterThanOrEqual(40);
    },
    10_000,
  );
});
