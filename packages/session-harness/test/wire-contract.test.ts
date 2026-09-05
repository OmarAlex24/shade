import { afterEach, describe, expect, test } from "bun:test";
import { createConnection } from "node:net";

import { PROTOCOL_VERSION } from "../../sdk-typescript/src/index.ts";
import { FakeShadeDaemon } from "../src/fake-daemon.ts";

let daemon: FakeShadeDaemon | undefined;

afterEach(async () => {
  await daemon?.stop();
  daemon = undefined;
});

describe("Rust v1 wire contract", () => {
  test("uses compact_context and wraps durable outcomes in OperationRecord", async () => {
    daemon = new FakeShadeDaemon(1_000);
    await daemon.start();

    const opened = await request(daemon.socket, {
      type: "execute",
      v: PROTOCOL_VERSION,
      request_id: "req-open",
      idempotency_key: "open:wire-contract",
      actor: { kind: "host", id: "wire-contract" },
      intent: {
        kind: "session_open",
        session_id: "session-opaque",
        repository: { kind: "local", path: "/repo" },
      },
    });

    expect(opened.status).toBe("ok");
    expect(opened.outcome.state).toBe("completed");
    expect(opened.outcome.result.compact_context.workspace).toBe(
      opened.outcome.result.workspace,
    );
    expect("context" in opened.outcome.result).toBe(false);
    expect(opened.outcome.result.env).toEqual({
      SHADE_SESSION: opened.outcome.result.session,
      SHADE_WORKSPACE: opened.outcome.result.workspace,
      SHADE_LEASE: opened.outcome.result.lease,
      SHADE_SOCKET: daemon.socket,
    });

    const accepted = await request(daemon.socket, {
      type: "execute",
      v: PROTOCOL_VERSION,
      request_id: "req-checkpoint",
      idempotency_key: "checkpoint:wire-contract",
      actor: { kind: "host", id: "wire-contract" },
      intent: {
        kind: "workspace_checkpoint",
        selector: {
          workspace_id: opened.outcome.result.workspace,
          cwd: opened.outcome.result.cwd,
        },
        reason: "contract",
      },
    });
    expect(accepted.outcome.state).toBe("accepted");

    const queried = await request(daemon.socket, {
      type: "query",
      v: PROTOCOL_VERSION,
      request_id: "req-operation",
      query: {
        kind: "operation",
        operation_id: accepted.outcome.result.operation_id,
      },
    });
    expect(queried.outcome.state).toBe("completed");
    const record = queried.outcome.result;
    expect(record).toMatchObject({
      id: accepted.outcome.result.operation_id,
      state: "completed",
      intent_kind: "workspace_checkpoint",
      error: null,
    });
    expect(record.outcome.state).toBe("completed");
    expect(record.outcome.result.checkpoint_id).toBeString();
    expect(record.outcome.result.head_sha).toBeString();
    expect("context" in record.outcome.result).toBe(false);
  });
});

async function request(socketPath: string, payload: unknown): Promise<any> {
  const socket = createConnection({ path: socketPath });
  socket.setEncoding("utf8");
  return await new Promise((resolve, reject) => {
    let input = "";
    socket.once("connect", () => socket.write(`${JSON.stringify(payload)}\n`));
    socket.on("data", (chunk: string) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline < 0) return;
      socket.destroy();
      try {
        resolve(JSON.parse(input.slice(0, newline)));
      } catch (error) {
        reject(error);
      }
    });
    socket.once("error", reject);
  });
}
