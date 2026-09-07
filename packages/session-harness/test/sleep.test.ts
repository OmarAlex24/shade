import { afterEach, expect, test } from "bun:test";
import { existsSync } from "node:fs";

import {
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
