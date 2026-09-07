import { afterEach, expect, test } from "bun:test";
import { createConnection } from "node:net";

import {
  PROTOCOL_VERSION,
  ShadeClient,
  ShadeError,
  type SessionStatus,
} from "../../sdk-typescript/src/index.ts";
import { FakeShadeDaemon } from "../src/fake-daemon.ts";

let daemon: FakeShadeDaemon | undefined;

afterEach(async () => {
  await daemon?.stop();
  daemon = undefined;
});

test("session_reattach renews the lease of a dormant session over the wire", async () => {
  daemon = new FakeShadeDaemon(60_000);
  await daemon.start();

  const opened = await request(daemon.socket, {
    type: "execute",
    v: PROTOCOL_VERSION,
    request_id: "req-open",
    idempotency_key: "open:reattach-wire",
    actor: { kind: "host", id: "reattach-wire" },
    intent: {
      kind: "session_open",
      session_id: "session-dormant",
      repository: { kind: "local", path: "/repo" },
    },
  });
  expect(opened.outcome.state).toBe("completed");

  daemon.expire("session-dormant");
  const dormant = await sessionQuery(daemon.socket, "session-dormant");
  expect(dormant.lifecycle).toBe("dormant");
  expect(dormant.lease).toBeUndefined();
  expect(dormant.materialized).toBe(true);

  const reattached = await request(daemon.socket, {
    type: "execute",
    v: PROTOCOL_VERSION,
    request_id: "req-reattach",
    idempotency_key: "reattach:reattach-wire",
    actor: { kind: "host", id: "reattach-wire" },
    intent: { kind: "session_reattach", session_id: "session-dormant" },
  });
  expect(reattached.outcome.state).toBe("completed");
  const payload = reattached.outcome.result;
  expect(payload.workspace).toBe(opened.outcome.result.workspace);
  expect(payload.cwd).toBe(opened.outcome.result.cwd);
  expect(payload.lease).not.toBe(opened.outcome.result.lease);
  expect(payload.env.SHADE_LEASE).toBe(payload.lease);

  const live = await sessionQuery(daemon.socket, "session-dormant");
  expect(live.lifecycle).toBe("active");
  expect(live.lease).toBe(payload.lease);
});

test("an expired lease is reattached under the caller's handle", async () => {
  const fake = new FakeShadeDaemon(60_000);
  daemon = fake;
  await fake.start();
  const client = new ShadeClient({
    socket: fake.socket,
    actor: { kind: "agent", id: "reattach-sdk" },
    heartbeat_interval_ms: 25,
  });
  expect(client.reattach_on_expiry).toBe(true);

  const session = await client.sessions.open({
    session_id: "chat-reattach",
    repository: { kind: "local", path: "/repo" },
  });
  const first = session.lease;
  const workspace = session.workspace;
  const cwd = session.cwd;
  fake.expire("chat-reattach");
  const beatsOnTheDeadLease = fake.heartbeat_counts.get(first) ?? 0;

  await waitUntil(() => session.lease !== first);

  // What changed: a new lease, exported under the same name, and the daemon
  // renewing that one rather than the dead one.
  expect(session.lease).not.toBe(first);
  expect(session.env.SHADE_LEASE).toBe(session.lease);
  await waitUntil(() => (fake.heartbeat_counts.get(session.lease) ?? 0) > 0);
  expect(fake.heartbeat_counts.get(first) ?? 0).toBe(beatsOnTheDeadLease);

  // What did not: a dormancy keeps the tree, so the handle keeps its identity
  // and its cwd instead of being handed a successor.
  expect(session.workspace).toBe(workspace);
  expect(session.cwd).toBe(cwd);

  const status = await client.sessions.status("chat-reattach");
  expect(status.lifecycle).toBe("active");
  expect(status.workspace).toBe(workspace);
  expect(status.lease).toBe(session.lease);
});

test("reattach can be turned off and the handle retires when its lease expires", async () => {
  daemon = new FakeShadeDaemon(60_000);
  await daemon.start();
  const client = new ShadeClient({
    socket: daemon.socket,
    actor: { kind: "agent", id: "no-reattach-sdk" },
    heartbeat_interval_ms: 25,
    reattach_on_expiry: false,
  });

  const session = await client.sessions.open({
    session_id: "chat-no-reattach",
    repository: { kind: "local", path: "/repo" },
  });
  const first = session.lease;
  daemon.expire("chat-no-reattach");

  await waitUntil(async () => {
    try {
      await session.context();
      return false;
    } catch (error) {
      return error instanceof ShadeError && error.code === "SESSION_HANDLE_RETIRED";
    }
  });
  expect(session.lease).toBe(first);

  const revived = await client.sessions.reattach("chat-no-reattach");
  expect(revived.workspace).toBe(session.workspace);
  expect(revived.lease).not.toBe(first);
});

async function waitUntil(
  condition: () => boolean | Promise<boolean>,
): Promise<void> {
  const deadline = Date.now() + 5_000;
  while (Date.now() < deadline) {
    if (await condition()) return;
    await Bun.sleep(10);
  }
  throw new Error("condition never held");
}

async function sessionQuery(
  socketPath: string,
  session_id: string,
): Promise<SessionStatus> {
  const response = await request(socketPath, {
    type: "query",
    v: PROTOCOL_VERSION,
    request_id: `req-status-${session_id}`,
    query: { kind: "session", session_id },
  });
  expect(response.status).toBe("ok");
  return response.outcome.result as SessionStatus;
}

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
