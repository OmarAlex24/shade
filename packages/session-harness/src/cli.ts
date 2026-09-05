#!/usr/bin/env bun

import { runSessionHarness } from "./harness.ts";

const report = await runSessionHarness();
process.stdout.write(`${JSON.stringify(report)}\n`);
