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
  // A session that never slept is simply reattached, and reattaching a live
  // session is idempotent: the same lease comes back with a full TTL rather
  // than a second one being minted under a handle that already holds one.
  expect(resumed.lease).toBe(lease);

  await expect(client.sessions.wake("chat-missing")).rejects.toBeInstanceOf(
    ShadeError,
  );
  await resumed.release();
});

/**
 * The harness is a contract, so every refusal it invents is a refusal a host
 * learns to handle and the daemon never sends -- and every refusal it misses is
 * one a host meets for the first time in production. These are the lifecycle
 * answers the engine gives, asked over the raw wire so no SDK retry or reattach
 * stands between the question and the code.
 */
test("the fake daemon refuses a sleep exactly where the daemon refuses one", async () => {
  daemon = new FakeShadeDaemon(60_000);
  await daemon.start();

  const opened = (await wire(daemon.socket, "open-dormant", {
    kind: "session_open",
    session_id: "chat-sleep-contract",
    repository: { kind: "local", path: "/repo" },
  })).outcome.result;

  // Sleeping needs no live lease: an idle workspace is the one worth
  // reclaiming disk from, and refusing a dormant one made a whole class of
  // sleeps impossible here that the daemon performs happily.
  daemon.expire("chat-sleep-contract");
  await Bun.write(`${opened.cwd}/build-output`, "x".repeat(4096));
  const slept = await wire(daemon.socket, "sleep-dormant", {
    kind: "workspace_sleep",
    selector: { workspace_id: opened.workspace, cwd: opened.cwd },
  });
  expect(slept.outcome.state).toBe("completed");
  expect(slept.outcome.result.reclaimed_bytes).toBe(4096);
  expect(existsSync(opened.cwd)).toBe(false);

  // Sleeping it again names the suspension and the command that ends it,
  // rather than reporting the cwd it no longer has.
  const again = await wire(daemon.socket, "sleep-again", {
    kind: "workspace_sleep",
    selector: { workspace_id: opened.workspace, cwd: opened.cwd },
  });
  expect(again.error.code).toBe("SESSION_SUSPENDED");
  expect(again.error.retry).toBe("never");
  expect(again.error.next).toContain("wake");

  // A suspension with no checkpoint has nothing to rebuild from.
  daemon.loseSuspensionCheckpoint("chat-sleep-contract");
  const unwakeable = await wire(daemon.socket, "wake-no-checkpoint", {
    kind: "session_wake",
    session_id: "chat-sleep-contract",
  });
  expect(unwakeable.error.code).toBe("SUSPENSION_CHECKPOINT_MISSING");
  expect(unwakeable.error.retry).toBe("never");
});

test("a released workspace and a superseded lease answer in the daemon's words", async () => {
  daemon = new FakeShadeDaemon(60_000);
  await daemon.start();

  const opened = (await wire(daemon.socket, "open-released", {
    kind: "session_open",
    session_id: "chat-released-contract",
    repository: { kind: "local", path: "/repo" },
  })).outcome.result;
  const released = await wire(daemon.socket, "release", {
    kind: "workspace_release",
    selector: { workspace_id: opened.workspace, cwd: opened.cwd },
  });
  // Release is accepted as a durable operation; what matters here is that the
  // record is gone by the time the next intent asks about it.
  expect(released.outcome.state).toBe("accepted");
  await waitUntil(
    async () =>
      (await sessionLifecycle(daemon!.socket, "chat-released-contract")) ===
      "released",
  );

  const resleep = await wire(daemon.socket, "sleep-released", {
    kind: "workspace_sleep",
    selector: { workspace_id: opened.workspace, cwd: opened.cwd },
  });
  // `WORKSPACE_RELEASED` is a code the engine has never emitted for anything.
  expect(resleep.error.code).toBe("WORKSPACE_ALREADY_RELEASED");

  // A superseded lease id matches no live row, which the daemon's heartbeat
  // cannot tell apart from an expiry -- it has exactly two refusals, and
  // `LEASE_FENCED` is not one of them.
  const fresh = (await wire(daemon.socket, "open-fenced", {
    kind: "session_open",
    session_id: "chat-fence-contract",
    repository: { kind: "local", path: "/repo" },
  })).outcome.result;
  const stale = await wire(daemon.socket, "heartbeat-stale", {
    kind: "lease_heartbeat",
    session_id: "chat-fence-contract",
    lease_id: `${fresh.lease}-superseded`,
  });
  expect(stale.error.code).toBe("LEASE_EXPIRED");
});

test("a workspace with work of its own in flight refuses to sleep", async () => {
  daemon = new FakeShadeDaemon(60_000);
  await daemon.start();
  const client = new ShadeClient({
    socket: daemon.socket,
    actor: { kind: "agent", id: "quiescence-sdk" },
    heartbeat_interval_ms: 60_000,
  });

  const session = await client.sessions.open({
    session_id: "chat-quiescence",
    repository: { kind: "local", path: "/repo" },
  });
  // The fake daemon hands back a conflict for a publish whose branch is taken,
  // which leaves a resolution workspace the parent is still going to want.
  const conflicted = await session.publish({
    branch: "conflict",
    message: "conflicting",
  });
  expect(conflicted.state).toBe("conflict");

  const refused = await wire(daemon.socket, "sleep-busy", {
    kind: "workspace_sleep",
    selector: { workspace_id: session.workspace, cwd: session.cwd },
  });
  expect(refused.error.code).toBe("WORKSPACE_NOT_QUIESCENT");
  expect(refused.error.retry).toBe("safe");
});

/** One raw request, with no SDK between the intent and the answer. */
async function wire(
  socketPath: string,
  requestId: string,
  intent: Record<string, unknown>,
): Promise<any> {
  const socket = createConnection({ path: socketPath });
  socket.setEncoding("utf8");
  const payload = {
    type: "execute",
    v: PROTOCOL_VERSION,
    request_id: requestId,
    idempotency_key: `${requestId}:key`,
    actor: { kind: "host", id: "wire-contract" },
    intent,
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

/** The lifecycle the daemon reports for one session, over the raw wire. */
async function sessionLifecycle(
  socketPath: string,
  sessionId: string,
): Promise<string> {
  const socket = createConnection({ path: socketPath });
  socket.setEncoding("utf8");
  const payload = {
    type: "query",
    v: PROTOCOL_VERSION,
    request_id: `lifecycle:${sessionId}`,
    query: { kind: "session", session_id: sessionId },
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
        resolve(JSON.parse(input.slice(0, newline)).outcome.result.lifecycle);
      } catch (error) {
        reject(error);
      }
    });
    socket.once("error", reject);
  });
}
