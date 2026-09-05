#!/usr/bin/env bun

import { runRealZenithHarness } from "./real-harness.ts";

try {
  const report = await runRealZenithHarness();
  process.stdout.write(`${JSON.stringify(report)}\n`);
} catch (error) {
  process.stdout.write(`${JSON.stringify({
    status: "failed",
    code: "REAL_HARNESS_FAILED",
    message: error instanceof Error ? error.message : String(error),
  })}\n`);
  process.exitCode = 1;
}
