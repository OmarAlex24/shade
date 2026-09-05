#!/usr/bin/env bun

import { runZenithHarness } from "./harness.ts";

const report = await runZenithHarness();
process.stdout.write(`${JSON.stringify(report)}\n`);
