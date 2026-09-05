import { expect, test } from "bun:test";
import { createServer } from "node:net";
import { mkdtemp, rm } from "node:fs/promises";
import { ShadeClient, ShadeError, type Diagnostic, type WireRequest } from "../src/index.ts";

test("diagnostics reads a typed record over IPC and preserves missing-record errors", async () => {
  const root = await mkdtemp("/private/tmp/shade-sdk-diag-");
  const socket = `${root}/s.sock`;
  const record: Diagnostic = {
    id: "diag_01J00000000000000000000000", origin: "daemon", operation: "op-failed",
    code: "INTERNAL", message: "<REDACTED: diagnostic contains credentials>",
    redacted: true, truncated: false, created_at_ms: 1,
  };
  const requests: WireRequest[] = [];
  const server = createServer((connection) => {
    let buffer = "";
    connection.on("data", (bytes) => {
      buffer += bytes.toString();
      if (!buffer.includes("\n")) return;
      const request: WireRequest = JSON.parse(buffer.slice(0, buffer.indexOf("\n")));
      requests.push(request);
      const found = request.type === "query" && request.query.kind === "diagnostics" && request.query.diagnostics_id === record.id;
      connection.end(JSON.stringify({ v: 1, request_id: request.request_id, ...(found
        ? { status: "ok", outcome: { state: "completed", result: record } }
        : { status: "error", error: { code: "DIAGNOSTIC_NOT_FOUND", retry: "never" } }) }) + "\n");
    });
  });
  try {
    await new Promise<void>((resolve, reject) => { server.once("error", reject); server.listen(socket, resolve); });
    const client = new ShadeClient({ socket, actor: { kind: "agent", id: "diagnostics-test" } });
    expect(await client.diagnostics(record.id)).toEqual(record);
    expect(requests[0]).toMatchObject({ type: "query", query: { kind: "diagnostics", diagnostics_id: record.id } });
    try { await client.diagnostics("missing"); throw new Error("expected missing record"); }
    catch (error) { expect(error).toBeInstanceOf(ShadeError); if (!(error instanceof ShadeError)) throw error; expect(error.code).toBe("DIAGNOSTIC_NOT_FOUND"); }
  } finally {
    await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
    await rm(root, { recursive: true, force: true });
  }
});
