import { afterEach, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { createConnection } from "node:net";

import {
  PROTOCOL_VERSION,
  ShadeClient,
  ShadeError,
  type SleepResult,
} from "../../sdk-typescript/src/index.ts";
import { FakeShadeDaemon } from "../src/fake-daemon.ts";

let daemon: FakeShadeDaemon | undefined;

afterEach(async () => {
  await daemon?.stop();
  daemon = undefined;
});

test("sleep frees the tree, keeps the session, and retires the handle", async () => {
  daemon = new FakeShadeDaemon(60_000);
  await daemon.start();
  const client = new ShadeClient({
    socket: daemon.socket,
    actor: { kind: "agent", id: "sleep-sdk" },
  });

  const session = await client.sessions.open({
    session_id: "chat-sleep",
    repository: { kind: "local", path: "/repo" },
  });
  const cwd = session.cwd;
  expect(existsSync(cwd)).toBe(true);

  const slept: SleepResult = await session.sleep();
  expect(slept.suspended).toBe(true);
  expect(slept.session).toBe("chat-sleep");
  expect(slept.workspace).toBe(session.workspace);
  expect(slept.checkpoint_id).toBeTruthy();
  expect(existsSync(cwd)).toBe(false);

  const status = await client.sessions.status("chat-sleep");
  expect(status.lifecycle).toBe("suspended");
  expect(status.materialized).toBe(false);
  expect(status.cwd).toBeUndefined();
  expect(status.lease).toBeUndefined();

  // Sleeping is not releasing, but the handle has no lease left to hold.
  await expect(session.context()).rejects.toMatchObject({
    code: "SESSION_HANDLE_RETIRED",
  });
});

test("wake rebuilds a suspended session as a successor workspace", async () => {
  daemon = new FakeShadeDaemon(60_000);
  await daemon.start();
  const client = new ShadeClient({
    socket: daemon.socket,
    actor: { kind: "agent", id: "wake-sdk" },
  });

  const opened = await client.sessions.open({
    session_id: "chat-wake",
    repository: { kind: "local", path: "/repo" },
  });
  const suspendedWorkspace = opened.workspace;
  await opened.sleep();

  const woken = await client.sessions.wake("chat-wake");
  expect(woken.session).toBe("chat-wake");
  expect(woken.workspace).not.toBe(suspendedWorkspace);
  expect(woken.cwd).not.toBe(opened.cwd);
  expect(existsSync(woken.cwd)).toBe(true);
  expect(woken.env.SHADE_SESSION).toBe("chat-wake");
  expect(woken.env.SHADE_WORKSPACE).toBe(woken.workspace);

  const status = await client.sessions.status("chat-wake");
  expect(status.lifecycle).toBe("active");
  expect(status.workspace).toBe(woken.workspace);
  await woken.release();
});

test("a handle whose session was slept elsewhere stops heartbeating", async () => {
  daemon = new FakeShadeDaemon(60_000);
  await daemon.start();
  const client = new ShadeClient({
    socket: daemon.socket,
    actor: { kind: "agent", id: "suspend-elsewhere-sdk" },
    heartbeat_interval_ms: 25,
  });

  const session = await client.sessions.open({
    session_id: "chat-slept-elsewhere",
    repository: { kind: "local", path: "/repo" },
  });

  // Slept over the wire rather than through the handle, which is how a
  // suspension actually reaches a background heartbeat: some other process
  // reclaimed the disk. The lease is not fenced and the workspace is not
  // released, so nothing but `SESSION_SUSPENDED` tells this handle to stop.
  const slept = await wireSleep(daemon.socket, session.workspace, session.cwd);
  expect(slept).toMatchObject({ status: "ok" });
  const beats = daemon.heartbeat_count;

  await waitUntil(async () => {
    try {
      await session.context();
      return false;
    } catch (error) {
      return (
        error instanceof ShadeError && error.code === "SESSION_HANDLE_RETIRED"
      );
    }
  });
  expect(daemon.heartbeat_count).toBe(beats);
  expect((await client.sessions.status("chat-slept-elsewhere")).lifecycle).toBe(
    "suspended",
  );
});

test("wake is safe to call on a session that never slept", async () => {
  daemon = new FakeShadeDaemon(60_000);
  await daemon.start();
  const client = new ShadeClient({
    socket: daemon.socket,
    actor: { kind: "agent", id: "wake-live-sdk" },
  });

  const session = await client.sessions.open({
    session_id: "chat-never-slept",
    repository: { kind: "local", path: "/repo" },
  });
  const workspace = session.workspace;
  const lease = session.lease;

  const resumed = await client.sessions.wake("chat-never-slept");
  expect(resumed.workspace).toBe(workspace);
  expect(resumed.lease).not.toBe(lease);

  await expect(client.sessions.wake("chat-missing")).rejects.toBeInstanceOf(
    ShadeError,
  );
  await resumed.release();
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

/** Sleep a workspace over the raw wire, without going through its handle. */
async function wireSleep(
  socketPath: string,
  workspace: string,
  cwd: string,
): Promise<any> {
  const socket = createConnection({ path: socketPath });
  socket.setEncoding("utf8");
  const payload = {
    type: "execute",
    v: PROTOCOL_VERSION,
    request_id: "req-sleep-elsewhere",
    idempotency_key: `sleep:${workspace}`,
    actor: { kind: "host", id: "sleep-elsewhere" },
    intent: {
      kind: "workspace_sleep",
      selector: { workspace_id: workspace, cwd },
    },
  };
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
