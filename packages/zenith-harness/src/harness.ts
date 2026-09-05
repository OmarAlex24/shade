import assert from "node:assert/strict";
import { existsSync } from "node:fs";

import {
  ShadeClient,
  ShadeTimeoutError,
  type CheckpointResult,
  type EventEnvelope,
  type OperationOutcome,
  type ShadeSession,
  type TerminalOutcome,
} from "../../sdk-typescript/src/index.ts";
import { FakeShadeDaemon } from "./fake-daemon.ts";

export interface HarnessReport {
  chats: number;
  initial_leases: number;
  handoff_leases: number;
  final_leases: number;
  events: number;
  heartbeats: number;
  ttl_verified: boolean;
  reconnects_tested: number;
  recovered_operation: boolean;
  selector_checks: number;
  cleanup: "clean";
}

export async function runZenithHarness(): Promise<HarnessReport> {
  const daemon = new FakeShadeDaemon(80);
  await daemon.start();
  const abortEvents = new AbortController();
  const received: EventEnvelope[] = [];
  let streamTask: Promise<void> | undefined;
  let stopped = false;

  try {
    const client = new ShadeClient({
      socket: daemon.socket,
      actor: { kind: "zenith", id: "zenith-harness" },
      timeout_ms: 2_000,
      operation_poll_ms: 1,
      event_reconnect_ms: 2,
      heartbeat_interval_ms: 10,
    });

    streamTask = (async () => {
      for await (const event of client.events(0, {
        signal: abortEvents.signal,
        reconnect_ms: 2,
      })) {
        received.push(event);
      }
    })();

    const chats = Array.from({ length: 20 }, (_, index) =>
      `chat-${index.toString().padStart(2, "0")}`,
    );
    const sessions = await Promise.all(
      chats.map((session_id) =>
        client.sessions.open({
          session_id,
          repository: { kind: "local", path: "/repo" },
          intent: `work:${session_id}`,
        }, { idempotency_key: `open:${session_id}` }),
      ),
    );

    assert.equal(daemon.active_leases, 20);
    assert.equal(new Set(sessions.map((session) => session.session)).size, 20);
    assert.equal(new Set(sessions.map((session) => session.workspace)).size, 20);
    assert.equal(new Set(sessions.map((session) => session.lease)).size, 20);
    assert.ok(daemon.idempotency_keys.has("open:chat-00"));
    for (const session of sessions) {
      assert.equal(session.env.SHADE_SESSION, session.session);
      assert.equal(session.env.SHADE_WORKSPACE, session.workspace);
      assert.equal(session.env.SHADE_LEASE, session.lease);
      assert.equal(session.env.SHADE_SOCKET, daemon.socket);
      assert.equal("SHADE_SESSION_ID" in session.env, false);
      assert.equal(session.compact_context.session, session.session);
      assert.equal(session.compact_context.workspace, session.workspace);
      assert.equal((await session.context()).workspace, session.workspace);
    }

    const initialLeases = sessions.map((session) => session.lease);
    await sleep(daemon.lease_ttl_ms * 2);
    assert.equal(daemon.active_leases, 20);
    for (const lease of initialLeases) {
      assert.ok((daemon.heartbeat_counts.get(lease) ?? 0) > 0);
    }

    await waitUntil(() => received.length >= 20);
    await daemon.crashAndRestart();

    const checkpoints = await Promise.all(
      sessions.map((session) => session.checkpoint("agent-turn")),
    );
    const checkpointIds = checkpoints.map(
      (outcome) => expectCompleted(outcome).checkpoint_id,
    );

    daemon.operation_query_delay_ms = 60;
    let timedOutOperation: string | undefined;
    try {
      await sessions[0]!.checkpoint("timeout-recovery", {
        timeout_ms: 10,
        idempotency_key: "checkpoint:timeout-recovery",
      });
      assert.fail("checkpoint should time out while its durable operation continues");
    } catch (error) {
      assert.ok(error instanceof ShadeTimeoutError);
      assert.equal(error.retry, "query_operation");
      assert.ok(error.operation_id);
      timedOutOperation = error.operation_id;
    }
    daemon.operation_query_delay_ms = 0;
    const recovered = await client.operations.wait<CheckpointResult>(
      timedOutOperation!,
    );
    assert.ok(expectCompleted(recovered).checkpoint_id);
    const recoveredRecord = await client.operations.getByKey<CheckpointResult>(
      "checkpoint:timeout-recovery",
    );
    assert.equal(recoveredRecord.id, timedOutOperation);
    assert.equal(recoveredRecord.state, "completed");
    assert.equal(recoveredRecord.error, null);
    assert.equal(recoveredRecord.outcome?.state, "completed");

    const parent = sessions[0]!;
    let successor = await parent.fork({
      child_session_id: "chat-00-successor",
      intent: "handoff",
    });
    assert.equal(successor.session, "chat-00-successor");
    assert.notEqual(successor.workspace, parent.workspace);
    assert.equal(daemon.active_leases, 21);

    assert.equal((await successor.context()).session, successor.session);
    const beforeSync = successor;
    successor = expectCompleted(await beforeSync.sync());
    assert.equal(successor.session, beforeSync.session);
    assert.notEqual(successor.workspace, beforeSync.workspace);
    assert.notEqual(successor.lease, beforeSync.lease);
    await expectRetired(beforeSync);
    await assertHeartbeatTransferred(daemon, beforeSync.lease, successor.lease);

    const beforeRestore = successor;
    successor = expectCompleted(await beforeRestore.restore(checkpointIds[0]!));
    assert.equal(successor.session, beforeRestore.session);
    assert.notEqual(successor.workspace, beforeRestore.workspace);
    await expectRetired(beforeRestore);
    await assertHeartbeatTransferred(daemon, beforeRestore.lease, successor.lease);

    const beforeRefresh = successor;
    successor = expectCompleted(await beforeRefresh.refreshDependencies());
    assert.equal(successor.compact_context.dependencies.state, "ready");
    assert.deepEqual(successor.compact_context.dependencies.providers, ["python"]);
    await expectRetired(beforeRefresh);
    await assertHeartbeatTransferred(daemon, beforeRefresh.lease, successor.lease);

    const published = expectCompleted(
      await successor.publish({
        branch: "feature/successor",
        message: "publish successor",
        push: true,
      }),
    );
    assert.equal(published.branch, "feature/successor");
    assert.equal(published.pushed, true);

    const conflict = await sessions[1]!.publish({
      branch: "conflict",
      message: "exercise conflict",
    });
    assert.equal(conflict.state, "conflict");
    if (conflict.state === "conflict") {
      assert.notEqual(conflict.result.workspace, sessions[1]!.workspace);
      assert.notEqual(conflict.result.cwd, sessions[1]!.cwd);
      assert.deepEqual(conflict.result.paths, ["src/conflict.ts"]);
    }

    expectCompleted(await parent.release());
    assert.equal(daemon.active_leases, 20);

    const secretPublish = expectCompleted(await sessions[2]!.publish({
      branch: "needs-review",
      message: "exercise secret review",
    }));
    assert.equal(secretPublish.branch, "needs-review");
    const review = await sessions[2]!.release();
    assert.equal(review.state, "review_required");
    if (review.state === "review_required") {
      const resolution = expectCompleted(
        await client.reviews.resolve(review.result.review_id, "keep"),
      );
      assert.ok("resolution" in resolution);
      if ("resolution" in resolution) {
        assert.equal(resolution.review, review.result.review_id);
        assert.equal(resolution.resolution, "kept");
      }
      await expectRetired(sessions[2]!);
    }

    assert.equal(daemon.active_leases, 19);
    await Promise.all([
      ...sessions
        .slice(1)
        .filter((session) => session !== sessions[2])
        .map((session) => session.release()),
      successor.release(),
    ]);
    assert.equal(daemon.active_leases, 0);
    assert.equal(daemon.resource_count, 0);
    const heartbeatCountAfterRelease = daemon.heartbeat_count;
    await sleep(35);
    assert.equal(daemon.heartbeat_count, heartbeatCountAfterRelease);

    await waitUntil(
      () => received.at(-1)?.cursor === daemon.latest_cursor,
      2_000,
    );
    abortEvents.abort();
    await streamTask;

    const cursors = received.map((event) => event.cursor);
    assert.equal(new Set(cursors).size, cursors.length);
    assert.deepEqual(
      cursors,
      Array.from({ length: daemon.latest_cursor }, (_, index) => index + 1),
    );
    assert.ok(daemon.selector_checks >= 20 * 2);

    const report: HarnessReport = {
      chats: chats.length,
      initial_leases: 20,
      handoff_leases: 20,
      final_leases: daemon.active_leases,
      events: received.length,
      heartbeats: daemon.heartbeat_count,
      ttl_verified: true,
      reconnects_tested: 1,
      recovered_operation: timedOutOperation !== undefined,
      selector_checks: daemon.selector_checks,
      cleanup: "clean",
    };

    const root = daemon.root;
    await daemon.stop();
    stopped = true;
    assert.equal(existsSync(root), false);
    return report;
  } finally {
    abortEvents.abort();
    if (streamTask !== undefined) await streamTask.catch(() => undefined);
    if (!stopped) await daemon.stop();
  }
}

function expectCompleted<T>(outcome: TerminalOutcome<T>): T {
  assert.equal(outcome.state, "completed");
  return (outcome as Extract<OperationOutcome<T>, { state: "completed" }>).result;
}

async function expectRetired(session: ShadeSession): Promise<void> {
  await assert.rejects(session.context(), (error: unknown) => {
    return (
      typeof error === "object" &&
      error !== null &&
      "code" in error &&
      error.code === "SESSION_HANDLE_RETIRED"
    );
  });
}

async function assertHeartbeatTransferred(
  daemon: FakeShadeDaemon,
  oldLease: string,
  newLease: string,
): Promise<void> {
  await sleep(15);
  const oldCount = daemon.heartbeat_counts.get(oldLease) ?? 0;
  await waitUntil(() => (daemon.heartbeat_counts.get(newLease) ?? 0) > 0);
  await sleep(25);
  assert.equal(daemon.heartbeat_counts.get(oldLease) ?? 0, oldCount);
}

async function waitUntil(predicate: () => boolean, timeoutMs = 1_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() >= deadline) assert.fail("condition was not reached before timeout");
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
}

async function sleep(ms: number): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, ms));
}
