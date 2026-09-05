import { describe, expect, test } from "bun:test";
import { createServer } from "node:net";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { ShadeClient, type WireRequest } from "../../sdk-typescript/src/index.ts";

import { runGc, runRealZenithHarness } from "../src/real-harness.ts";

describe("real Zenith lifecycle harness", () => {
  test("continues the durable GC operation after CLI exit 75", async () => {
    const root = await mkdtemp("/private/tmp/shade-harness-gc-");
    const socket = `${root}/s.sock`;
    const binary = `${root}/shade`;
    const operation = "op-gc-timeout";
    const result = { eligible: 2, deleted: 2, skipped: 0, layers_deleted: 0, layers_reclaimed: 0 };
    const response = { v: 1, request_id: "cli", status: "error", error: { code: "CLIENT_TIMEOUT", retry: "query_operation", operation } };
    await writeFile(binary, `#!/bin/sh\ncat <<'SHADE_RESPONSE'\n${JSON.stringify(response)}\nSHADE_RESPONSE\nexit 75\n`, { mode: 0o700 });
    const requests: WireRequest[] = [];
    const server = createServer((connection) => {
      let buffer = "";
      connection.on("data", (data) => {
        buffer += data.toString();
        if (!buffer.includes("\n")) return;
        const request: WireRequest = JSON.parse(buffer.slice(0, buffer.indexOf("\n")));
        requests.push(request);
        connection.end(JSON.stringify({ v: 1, request_id: request.request_id, status: "ok", outcome: { state: "completed", result: { id: operation, intent_kind: "garbage_collect", state: "completed", outcome: { state: "completed", result }, error: null, created_at_ms: 1, updated_at_ms: 2 } } }) + "\n");
      });
    });
    try {
      await new Promise<void>((resolve, reject) => { server.once("error", reject); server.listen(socket, resolve); });
      const client = new ShadeClient({ socket, actor: { kind: "agent", id: "gc-recovery-test" } });
      expect(await runGc(binary, root, socket, client, "gc-test")).toEqual(result);
      expect(requests).toHaveLength(1);
      expect(requests[0]).toMatchObject({ type: "query", query: { kind: "operation", operation_id: operation } });
    } finally {
      await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
      await rm(root, { recursive: true, force: true });
    }
  });

  test("rejects a missing Shade binary before creating state", async () => {
    await expect(
      runRealZenithHarness({ shade_bin: "/definitely/missing/shade" }),
    ).rejects.toBeDefined();
  });

  const realTest = process.env.SHADE_BIN === undefined ? test.skip : test;
  realTest(
    "drives twenty chats through the real binary and leaves no resources",
    async () => {
      const report = await runRealZenithHarness();
      expect(report).toMatchObject({
        mode: "real",
        package_manager: "host_npm",
        source_fetches: 1,
        dependency_forest_roots: 2,
        dependency_install_commands: 2,
        dependency_offline_commands: 1,
        dependency_tarball_requests: 2,
        chats: 20,
        repository_stores: 1,
        initial_workspaces: 20,
        dependency_preparations: 1,
        dependency_layers: 1,
        dependency_fingerprints: 1,
        dependency_receipts: 20,
        dependency_isolation: true,
        checkpoints: 20,
        forked: true,
        synced_and_adopted: true,
        published: true,
        daemon_restarts: 1,
        sqlite_inode_preserved: true,
        event_reconnects: 1,
        doctor: {
          state: "ok",
          sessions: 0,
          workspaces: 0,
          operations: 0,
        },
        cleanup: {
          status: "clean",
          active_leases: 0,
          live_workspaces: 0,
          linked_worktrees: 0,
          private_refs: 0,
          dependency_staging_entries: 0,
          dependency_artifacts: 1,
          dependency_mutable_entries: 0,
        },
      });
      expect(report.heartbeat_leases).toBeGreaterThanOrEqual(20);
      expect(report.binary_sha256).toMatch(/^[a-f0-9]{64}$/);
      expect(report.tools.map((tool) => tool.name)).toEqual(["npm", "node"]);
      expect(report.events).toBe(report.event_cursor);
    },
    900_000,
  );
});
