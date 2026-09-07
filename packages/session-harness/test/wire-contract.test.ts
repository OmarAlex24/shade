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

  test("carries workspace_sleep and session_wake as bare wire intents", async () => {
    daemon = new FakeShadeDaemon(1_000);
    await daemon.start();

    const opened = await request(daemon.socket, {
      type: "execute",
      v: PROTOCOL_VERSION,
      request_id: "req-open-sleep",
      idempotency_key: "open:wire-sleep",
      actor: { kind: "host", id: "wire-contract" },
      intent: {
        kind: "session_open",
        session_id: "session-suspends",
        repository: { kind: "local", path: "/repo" },
      },
    });
    const suspended = opened.outcome.result;

    const slept = await request(daemon.socket, {
      type: "execute",
      v: PROTOCOL_VERSION,
      request_id: "req-sleep",
      idempotency_key: "sleep:wire-sleep",
      actor: { kind: "host", id: "wire-contract" },
      intent: {
        kind: "workspace_sleep",
        selector: { workspace_id: suspended.workspace, cwd: suspended.cwd },
      },
    });

    expect(slept.status).toBe("ok");
    expect(slept.outcome.state).toBe("completed");
    // SleepResult is its own shape: no compact_context, because a suspended
    // workspace has no tree left to describe.
    expect(slept.outcome.result).toMatchObject({
      session: "session-suspends",
      workspace: suspended.workspace,
      suspended: true,
    });
    expect(slept.outcome.result.checkpoint_id).toBeString();
    expect(slept.outcome.result.reclaimed_bytes).toBeNumber();
    // The caller may be standing in the directory sleep just removed, so the
    // result names it and the one line that recovers from it.
    expect(slept.outcome.result.cwd).toBe(suspended.cwd);
    expect(slept.outcome.result.next).toBe(
      "cd to another directory, then: shade wake --session session-suspends",
    );
    expect("compact_context" in slept.outcome.result).toBe(false);
    expect("context" in slept.outcome.result).toBe(false);

    const woken = await request(daemon.socket, {
      type: "execute",
      v: PROTOCOL_VERSION,
      request_id: "req-wake",
      idempotency_key: "wake:wire-sleep",
      actor: { kind: "host", id: "wire-contract" },
      intent: { kind: "session_wake", session_id: "session-suspends" },
    });

    expect(woken.outcome.state).toBe("completed");
    // Wake answers with the same OpenedSession shape `session_open` does, so a
    // host can hand the result straight back to whatever it opened with.
    const successor = woken.outcome.result;
    expect(successor.session).toBe("session-suspends");
    expect(successor.workspace).not.toBe(suspended.workspace);
    expect(successor.compact_context.workspace).toBe(successor.workspace);
    // `lifecycle` is omitted when it is `active`, so a woken session looks
    // exactly like a freshly opened one on the wire.
    expect("lifecycle" in successor.compact_context).toBe(false);
    expect("context" in successor).toBe(false);
    expect(successor.env).toEqual({
      SHADE_SESSION: successor.session,
      SHADE_WORKSPACE: successor.workspace,
      SHADE_LEASE: successor.lease,
      SHADE_SOCKET: daemon.socket,
    });
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
