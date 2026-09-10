import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import {
  chmod,
  copyFile,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createHostNpmFixture, type HostToolEvidence, type NpmFixture } from "./host-npm.ts";
import {
  COMMAND_TIMEOUT_MS,
  DaemonProcess,
  HARNESS_LEASE_TTL_SECS,
  HARNESS_ORPHAN_GRACE_SECS,
  type DoctorResult,
  directoryNames,
  expectCompleted,
  git,
  isObject,
  pathExists,
  readDirectory,
  resolveShadeBinary,
  runCommand,
  runDoctor,
  runGc,
  runShadeOperation,
  sleep,
  waitForExit,
  waitUntil,
} from "./real-daemon.ts";

import {
  ShadeClient,
  type CheckpointResult,
  type EventEnvelope,
  type OpenedSessionPayload,
  type ShadeSession,
  type SleepResult,
} from "../../sdk-typescript/src/index.ts";

const CHAT_COUNT = 20 as const;

export interface RealHarnessOptions {
  shade_bin?: string;
  keep_root?: boolean;
}

export interface RealHarnessReport {
  mode: "real";
  binary_sha256: string;
  package_manager: "host_npm";
  tools: HostToolEvidence[];
  source_fetches: 1;
  dependency_forest_roots: 2;
  dependency_install_commands: 2;
  dependency_offline_commands: 1;
  dependency_tarball_requests: 2;
  chats: 20;
  repository_stores: 1;
  initial_workspaces: 20;
  dependency_preparations: 1;
  dependency_layers: 1;
  dependency_fingerprints: 1;
  dependency_receipts: 20;
  dependency_isolation: true;
  checkpoints: 20;
  forked: true;
  synced_and_adopted: true;
  published: true;
  daemon_restarts: 1;
  sqlite_inode_preserved: true;
  keepalive: {
    spawned: true;
    detached: true;
    stopped_on_owner_exit: true;
  };
  suspended_and_woken: true;
  heartbeat_leases: number;
  events: number;
  event_reconnects: 1;
  event_cursor: number;
  lease_ttl_secs: 120;
  orphan_grace_secs: 0;
  gc_rounds: number;
  doctor: DoctorResult;
  cleanup: CleanupInventory & { status: "clean" };
}

/** The CLI's private keepalive pidfile, as written by `keepalive::registry`. */
interface KeepaliveRecord {
  v: number;
  session: string;
  lease: string;
  keepalive_pid: number;
  owner_pid: number;
  owner_name: string;
}

interface DatabaseInventory {
  active_sessions: number;
  active_leases: number;
  live_workspaces: number;
  running_operations: number;
  pending_handoffs: number;
  pending_reviews: number;
  dependency_receipts: number;
}

interface CleanupInventory extends DatabaseInventory {
  workspace_entries: number;
  linked_worktrees: number;
  private_refs: number;
  dependency_staging_entries: number;
  dependency_artifacts: number;
  dependency_mutable_entries: number;
  temporary_entries: number;
}

export async function runRealSessionHarness(
  options: RealHarnessOptions = {},
): Promise<RealHarnessReport> {
  const sourceBinary = await resolveShadeBinary(options.shade_bin);
  const keepRoot = options.keep_root ?? process.env.SHADE_HARNESS_KEEP_ROOT === "1";
  assert.equal(process.platform, "darwin", "real harness requires macOS");
  assert.equal(process.arch, "arm64", "real harness requires Apple Silicon");
  await runCommand("git", ["--version"]);
  await runCommand("/usr/bin/sqlite3", ["--version"]);

  const harnessRoot = await realpath(await mkdtemp(join(tmpdir(), "shade-real-harness-")));
  const stateRoot = join(harnessRoot, "state");
  const socket = join(stateRoot, "shade.sock");
  const repository = join(harnessRoot, "fixture");
  const shadeBin = join(harnessRoot, "shade");
  const gitTrace = join(harnessRoot, "git-trace.jsonl");
  const runId = randomUUID().replaceAll("-", "").slice(0, 12);
  const actorId = `host-real-${runId}`;
  const received: EventEnvelope[] = [];
  const abortEvents = new AbortController();
  let eventError: unknown;
  let daemon: DaemonProcess | undefined;
  let eventTask: Promise<void> | undefined;
  let npmFixture: NpmFixture | undefined;

  try {
    await copyFile(sourceBinary, shadeBin);
    await chmod(shadeBin, 0o500);
    const binaryDigest = createHash("sha256").update(await readFile(shadeBin)).digest("hex");
    await createGitFixture(repository);
    npmFixture = await createHostNpmFixture(repository, harnessRoot, runCommand);
    await git(repository, ["add", "--all"]);
    await git(repository, ["commit", "-m", "real npm workspace fixture"]);
    daemon = await DaemonProcess.start(shadeBin, stateRoot, socket, {
      env: { GIT_TRACE2_EVENT: gitTrace },
    });

    const client = new ShadeClient({
      socket,
      actor: { kind: "host", id: actorId },
      timeout_ms: COMMAND_TIMEOUT_MS,
      operation_poll_ms: 10,
      event_reconnect_ms: 25,
      heartbeat_interval_ms: 500,
    });
    eventTask = (async () => {
      try {
        for await (const event of client.events(0, {
          signal: abortEvents.signal,
          reconnect_ms: 25,
        })) {
          received.push(event);
        }
      } catch (error) {
        eventError = error;
      }
    })();

    const chats = Array.from(
      { length: CHAT_COUNT },
      (_, index) => `real-chat-${runId}-${index.toString().padStart(2, "0")}`,
    );
    const sessions = await Promise.all(
      chats.map((sessionId) =>
        client.sessions.open(
          {
            session_id: sessionId,
            repository: { kind: "local", path: repository },
            base: "main",
            intent: `real-harness:${sessionId}`,
          },
          { idempotency_key: `open:${sessionId}` },
        ),
      ),
    );

    assert.equal(new Set(sessions.map((session) => session.session)).size, CHAT_COUNT);
    assert.equal(new Set(sessions.map((session) => session.workspace)).size, CHAT_COUNT);
    assert.equal(new Set(sessions.map((session) => session.lease)).size, CHAT_COUNT);
    assert.equal((await directoryNames(join(stateRoot, "repositories"))).length, 1);
    assert.equal((await directoryNames(join(stateRoot, "workspaces"))).length, CHAT_COUNT);
    assert.equal(await countBaseTrees(join(stateRoot, "bases")), 1);
    assert.equal((await databaseInventory(stateRoot)).dependency_receipts, CHAT_COUNT);
    assert.equal(
      await databaseScalar(
        stateRoot,
        "SELECT COUNT(DISTINCT fingerprint) FROM dependency_receipts;",
      ),
      1,
      "twenty opens must share one dependency fingerprint",
    );
    assert.equal(
      await databaseScalar(
        stateRoot,
        "SELECT COUNT(DISTINCT provider) FROM dependency_receipts;",
      ),
      1,
      "the fixture must select only npm",
    );
    const dependencyArtifacts = await artifactPaths(
      join(stateRoot, "dependencies", "artifacts"),
    );
    assert.equal(dependencyArtifacts.length, 1);
    const installInvocations = await npmInstallCommands(stateRoot);
    assert.equal(installInvocations.length, 2, "twenty opens must share one npm fill/replay");
    assert.equal(
      installInvocations.filter((line) => line.includes("--offline")).length,
      1,
      "the shared preparation must contain exactly one offline replay",
    );
    assert.ok(installInvocations.every((line) => line.includes("--ignore-scripts")));
    assert.deepEqual(npmFixture.tarball_requests(), [1, 1]);
    assert.equal(await sourceFetchCount(gitTrace, repository), 1);
    const receipt = JSON.parse(await readFile(join(dependencyArtifacts[0]!, "receipt.json"), "utf8"));
    assert.deepEqual(receipt.tools, npmFixture.tools);
    assert.deepEqual(receipt.materialized_paths, ["node_modules", "packages/member/node_modules"]);
    assert.equal(receipt.scripts.length, 2);
    assert.ok(receipt.scripts.every((script: { executed: boolean }) => !script.executed));
    for (const session of sessions) {
      assert.equal(session.env.SHADE_SESSION, session.session);
      assert.equal(session.env.SHADE_WORKSPACE, session.workspace);
      assert.equal(session.env.SHADE_LEASE, session.lease);
      assert.equal(session.env.SHADE_SOCKET, socket);
      assert.equal((await session.context()).workspace, session.workspace);
      assert.equal(
        await readFile(join(session.cwd, "node_modules", "shade-fixture", "index.js"), "utf8"),
        "export const shadeFixture = true;\n",
      );
      assert.equal(await readFile(join(session.cwd, "packages/member/node_modules/shade-fixture/index.js"), "utf8"), "export const shadeFixture = 2;\n");
      assert.equal(await realpath(join(session.cwd, "node_modules/shade-member")), join(session.cwd, "packages/member"));
    }

    const cachedDependency = join(
      dependencyArtifacts[0]!,
      "payload",
      "node_modules",
      "shade-fixture",
      "index.js",
    );
    const cachedDependencyBefore = await lstat(cachedDependency);
    const firstDependencyBefore = await lstat(
      join(sessions[0]!.cwd, "node_modules", "shade-fixture", "index.js"),
    );
    const secondDependencyBefore = await lstat(
      join(sessions[1]!.cwd, "node_modules", "shade-fixture", "index.js"),
    );
    assert.equal(firstDependencyBefore.dev, cachedDependencyBefore.dev);
    assert.equal(secondDependencyBefore.dev, cachedDependencyBefore.dev);
    assert.notEqual(firstDependencyBefore.ino, cachedDependencyBefore.ino);
    assert.notEqual(secondDependencyBefore.ino, cachedDependencyBefore.ino);
    assert.notEqual(firstDependencyBefore.ino, secondDependencyBefore.ino);
    await writeFile(
      join(sessions[0]!.cwd, "node_modules", "shade-fixture", "index.js"),
      "workspace-local mutation\n",
    );
    assert.equal(
      await readFile(join(sessions[1]!.cwd, "node_modules", "shade-fixture", "index.js"), "utf8"),
      "export const shadeFixture = true;\n",
    );
    assert.equal(
      await readFile(cachedDependency, "utf8"),
      "export const shadeFixture = true;\n",
    );
    const nestedRelative = "packages/member/node_modules/shade-fixture/index.js";
    await writeFile(join(sessions[0]!.cwd, nestedRelative), "nested workspace mutation\n");
    assert.equal(await readFile(join(sessions[1]!.cwd, nestedRelative), "utf8"), "export const shadeFixture = 2;\n");
    assert.equal(await readFile(join(dependencyArtifacts[0]!, "payload", nestedRelative), "utf8"), "export const shadeFixture = 2;\n");

    await writeFile(join(sessions[0]!.cwd, "agent-only.txt"), "agent state\n");
    assert.equal(await pathExists(join(sessions[1]!.cwd, "agent-only.txt")), false);
    await waitUntil(
      () => uniqueHeartbeatLeases(received) >= CHAT_COUNT,
      15_000,
      "automatic heartbeats for all twenty leases",
    );
    assert.equal(eventError, undefined);

    const databaseBefore = await lstat(join(stateRoot, "state.sqlite"));
    const restartBoundary = received.at(-1)?.cursor ?? 0;
    await daemon.crash();
    daemon = await DaemonProcess.start(shadeBin, stateRoot, socket, {
      env: { GIT_TRACE2_EVENT: gitTrace },
    });
    const databaseAfter = await lstat(join(stateRoot, "state.sqlite"));
    assert.equal(databaseAfter.dev, databaseBefore.dev);
    assert.equal(databaseAfter.ino, databaseBefore.ino);
    const postRestartDoctor = await runDoctor(shadeBin, stateRoot, socket);
    assert.equal(postRestartDoctor.state, "ok");
    assert.equal(postRestartDoctor.sessions, CHAT_COUNT);
    await Promise.all(sessions.map((session) => session.context()));

    // A twenty-first session, opened through the CLI, which is the case the
    // SDK cannot cover: a command that exits leaves nothing behind to hold its
    // lease. The production TTL is two minutes, so nothing here waits for
    // dormancy -- the keepalive is proven by the heartbeats it sends on its
    // own and by the pidfile disappearing the moment its owner dies.
    let gcRounds = 0;
    const keepaliveSession = `real-keepalive-${runId}`;
    const owner = spawn("/bin/sleep", ["600"], { stdio: "ignore" });
    const ownerPid = owner.pid;
    assert.ok(typeof ownerPid === "number", "fake owner process has no pid");
    const opened = await runShadeOperation<OpenedSessionPayload>(
      shadeBin,
      stateRoot,
      socket,
      client,
      [
        "--idempotency-key",
        `open:${keepaliveSession}`,
        "open",
        repository,
        "--session",
        keepaliveSession,
        "--base",
        "main",
        "--owner-pid",
        String(ownerPid),
        "--interval-secs",
        "1",
      ],
    );
    assert.equal(opened.session, keepaliveSession);
    assert.equal(opened.keepalive?.state, "started");
    assert.equal(opened.keepalive?.owner_pid, ownerPid);

    const pidfile = join(
      stateRoot,
      "runtime",
      "keepalive",
      `${createHash("sha256").update(keepaliveSession).digest("hex")}.json`,
    );
    assert.equal((await lstat(pidfile)).mode & 0o777, 0o600);
    const keepaliveRecord = JSON.parse(await readFile(pidfile, "utf8")) as KeepaliveRecord;
    assert.equal(keepaliveRecord.session, keepaliveSession);
    assert.equal(keepaliveRecord.lease, opened.lease);
    assert.equal(keepaliveRecord.owner_pid, ownerPid);
    assert.equal(keepaliveRecord.keepalive_pid, opened.keepalive?.pid);
    // It is still running after the `shade open` that spawned it returned.
    assert.equal(processIsAlive(keepaliveRecord.keepalive_pid), true);
    await waitUntil(
      () => received.filter(
        (event) => event.event === "lease.heartbeat" && event.resource === opened.lease,
      ).length >= 2,
      15_000,
      "the detached keepalive heartbeats without the harness asking it to",
    );

    owner.kill("SIGKILL");
    assert.equal(await waitForExit(owner, 5_000), true);
    await waitUntil(
      async () => !(await pathExists(pidfile)),
      5_000,
      "the keepalive retires its pidfile when its owner exits",
    );
    await waitUntil(
      () => !processIsAlive(keepaliveRecord.keepalive_pid),
      5_000,
      "the keepalive exits with the owner it was watching",
    );

    await writeFile(join(opened.cwd, "keepalive-only.txt"), "written before sleeping\n");
    const slept = await runShadeOperation<SleepResult>(shadeBin, stateRoot, socket, client, [
      "--idempotency-key",
      `sleep:${keepaliveSession}`,
      "sleep",
      "--workspace",
      opened.workspace,
    ]);
    assert.equal(slept.suspended, true);
    assert.equal(slept.workspace, opened.workspace);
    // `private_bytes` on APFS, so zero is a legitimate answer: a clone whose
    // every block is still shared with the immutable base frees nothing by
    // being removed. What the contract promises is a number, measured before
    // the tree goes, not that it is positive.
    assert.ok(
      Number.isFinite(slept.reclaimed_bytes) && slept.reclaimed_bytes >= 0,
      "sleep reports the disk it measured before removing the tree",
    );
    assert.equal(await pathExists(opened.cwd), false, "sleep gives up the tree");
    let suspendedDoctor = await runDoctor(shadeBin, stateRoot, socket);
    assert.equal(suspendedDoctor.workspaces_suspended, 1);
    assert.equal(suspendedDoctor.sessions_suspended, 1);
    assert.equal(suspendedDoctor.suspended_without_checkpoint, 0);
    assert.equal(suspendedDoctor.sessions, CHAT_COUNT);

    const suspendedGc = await runGc(shadeBin, stateRoot, socket, client, `gc:${runId}:suspended`);
    gcRounds += 1;
    assert.equal(suspendedGc.deleted, 0, "a suspended workspace is never a collector candidate");
    suspendedDoctor = await runDoctor(shadeBin, stateRoot, socket);
    assert.equal(suspendedDoctor.workspaces_suspended, 1);

    const woken = await runShadeOperation<OpenedSessionPayload>(
      shadeBin,
      stateRoot,
      socket,
      client,
      [
        "--idempotency-key",
        `wake:${keepaliveSession}`,
        "wake",
        "--session",
        keepaliveSession,
        "--no-keepalive",
      ],
    );
    assert.equal(woken.session, keepaliveSession);
    assert.notEqual(woken.workspace, opened.workspace);
    assert.notEqual(woken.cwd, opened.cwd);
    assert.equal(await pathExists(woken.cwd), true);
    assert.equal(
      await readFile(join(woken.cwd, "keepalive-only.txt"), "utf8"),
      "written before sleeping\n",
      "a file Git never tracked survives the round trip",
    );
    assert.equal(await readFile(join(woken.cwd, "README.md"), "utf8"), "# real Shade harness\n");
    assert.equal(
      await readFile(join(woken.cwd, "node_modules", "shade-fixture", "index.js"), "utf8"),
      "export const shadeFixture = true;\n",
      "the dependency layer is rebuilt from its recorded fingerprint, not reinstalled",
    );
    const wokenDoctor = await runDoctor(shadeBin, stateRoot, socket);
    assert.equal(wokenDoctor.workspaces_suspended, 0);
    assert.equal(wokenDoctor.sessions, CHAT_COUNT + 1);

    const releasedKeepalive = await runShadeOperation<{ released: boolean }>(
      shadeBin,
      stateRoot,
      socket,
      client,
      [
        "--idempotency-key",
        `release:${keepaliveSession}`,
        "release",
        "--workspace",
        woken.workspace,
      ],
    );
    assert.equal(releasedKeepalive.released, true);
    assert.equal((await runDoctor(shadeBin, stateRoot, socket)).sessions, CHAT_COUNT);

    const checkpointOutcomes = await Promise.all(
      sessions.map((session, index) =>
        session.checkpoint("real-harness", {
          idempotency_key: `checkpoint:${runId}:${index}`,
        }),
      ),
    );
    const checkpoints = checkpointOutcomes.map((outcome) =>
      expectCompleted<CheckpointResult>(outcome),
    );
    assert.equal(new Set(checkpoints.map((checkpoint) => checkpoint.checkpoint_id)).size, 20);
    await waitUntil(
      () => received.some(
        (event) => event.cursor > restartBoundary && event.event === "checkpoint.created",
      ),
      15_000,
      "event stream resume after daemon restart",
    );

    const parent = sessions[0]!;
    const forked = await parent.fork(
      {
        child_session_id: `real-fork-${runId}`,
        intent: "real fork fidelity",
      },
      { idempotency_key: `fork:${runId}` },
    );
    assert.notEqual(forked.workspace, parent.workspace);
    assert.equal(await readFile(join(forked.cwd, "agent-only.txt"), "utf8"), "agent state\n");
    assert.equal(await readFile(join(forked.cwd, nestedRelative), "utf8"), "nested workspace mutation\n");

    await writeFile(join(repository, "remote-update.txt"), "new base\n");
    await git(repository, ["add", "remote-update.txt"]);
    await git(repository, ["commit", "-m", "remote update"]);
    const predecessor = sessions[1]!;
    const synced = expectCompleted(await predecessor.sync({
      idempotency_key: `sync:${runId}`,
    }));
    assert.equal(synced.session, predecessor.session);
    assert.notEqual(synced.workspace, predecessor.workspace);
    assert.equal(await readFile(join(synced.cwd, "remote-update.txt"), "utf8"), "new base\n");
    assert.equal(await pathExists(join(predecessor.cwd, "remote-update.txt")), false);
    assert.equal(await pathExists(predecessor.cwd), true);
    await assert.rejects(predecessor.context(), isRetiredSessionError);

    await writeFile(join(forked.cwd, "published-by-agent.txt"), "publish me\n");
    const branch = `shade-harness-${runId}`;
    const published = expectCompleted(await forked.publish(
      { branch, message: "real harness publish", push: false },
      { idempotency_key: `publish:${runId}` },
    ));
    assert.equal(published.branch, branch);
    assert.equal(published.pushed, false);
    assert.ok(published.commit.length > 0);

    const managedRepositories = await managedRepositoryPaths(stateRoot);
    assert.equal(managedRepositories.length, 1);
    const privateRefsBeforeCleanup = await privateRefs(managedRepositories);
    assert.ok(privateRefsBeforeCleanup.length >= CHAT_COUNT * 3);

    const liveSessions = [
      ...sessions.filter((_, index) => index !== 1),
      synced,
      forked,
    ];
    assert.equal(new Set(liveSessions.map((session) => session.workspace)).size, 21);
    const releases = await Promise.all(
      liveSessions.map((session, index) =>
        session.release({ idempotency_key: `release:${runId}:${index}` }),
      ),
    );
    for (const release of releases) {
      assert.equal(expectCompleted(release).released, true);
    }
    let doctor = await runDoctor(shadeBin, stateRoot, socket);
    assert.equal(doctor.sessions, 0);
    assert.equal((await databaseInventory(stateRoot)).active_leases, 0);

    await runGc(shadeBin, stateRoot, socket, client, `gc:${runId}:initial`);
    gcRounds += 1;
    doctor = await runDoctor(shadeBin, stateRoot, socket);
    for (let round = 0; doctor.workspaces > 0 && round < 3; round += 1) {
      await sleep(25);
      await runGc(shadeBin, stateRoot, socket, client, `gc:${runId}:final:${round}`);
      gcRounds += 1;
      doctor = await runDoctor(shadeBin, stateRoot, socket);
    }
    assert.equal(doctor.state, "ok");
    assert.equal(doctor.sessions, 0);
    assert.equal(
      doctor.workspaces,
      0,
      `GC left ${doctor.workspaces} workspaces with the explicit zero-second harness grace; all real session, restart, checkpoint, fork, sync/adopt, publish and release flows completed before cleanup`,
    );
    assert.equal(doctor.operations, 0);

    const latestCursor = await databaseScalar(
      stateRoot,
      "SELECT COALESCE(MAX(cursor), 0) FROM events;",
    );
    await waitUntil(
      () => (received.at(-1)?.cursor ?? 0) === latestCursor,
      15_000,
      "event stream catch-up before shutdown",
    );
    abortEvents.abort();
    await eventTask;
    assert.equal(eventError, undefined);
    assert.deepEqual(
      received.map((event) => event.cursor),
      Array.from({ length: latestCursor }, (_, index) => index + 1),
    );

    await daemon.stop();
    daemon = undefined;
    const cleanup = await cleanupInventory(stateRoot);
    assert.deepEqual(cleanup, {
      active_sessions: 0,
      active_leases: 0,
      live_workspaces: 0,
      running_operations: 0,
      pending_handoffs: 0,
      pending_reviews: 0,
      dependency_receipts: 0,
      workspace_entries: 0,
      linked_worktrees: 0,
      private_refs: 0,
      dependency_staging_entries: 0,
      dependency_artifacts: 1,
      dependency_mutable_entries: 0,
      temporary_entries: 0,
    });
    assert.deepEqual(
      await artifactPaths(join(stateRoot, "dependencies", "artifacts")),
      dependencyArtifacts,
      "cleanup must retain exactly the immutable npm layer",
    );
    const cachedDependencyAfter = await lstat(cachedDependency);
    assert.equal(cachedDependencyAfter.dev, cachedDependencyBefore.dev);
    assert.equal(cachedDependencyAfter.ino, cachedDependencyBefore.ino);
    assert.equal(
      await readFile(cachedDependency, "utf8"),
      "export const shadeFixture = true;\n",
      "agent writes must never be promoted back into the retained layer",
    );
    assert.deepEqual(await npmInstallCommands(stateRoot), installInvocations, "restart and successors must reuse the same preparation");
    assert.deepEqual(npmFixture.tarball_requests(), [1, 1]);
    assert.equal(createHash("sha256").update(await readFile(shadeBin)).digest("hex"), binaryDigest);

    return {
      mode: "real",
      binary_sha256: binaryDigest,
      package_manager: "host_npm",
      tools: npmFixture.tools,
      source_fetches: 1,
      dependency_forest_roots: 2,
      dependency_install_commands: 2,
      dependency_offline_commands: 1,
      dependency_tarball_requests: 2,
      chats: CHAT_COUNT,
      repository_stores: 1,
      initial_workspaces: CHAT_COUNT,
      dependency_preparations: 1,
      dependency_layers: 1,
      dependency_fingerprints: 1,
      dependency_receipts: CHAT_COUNT,
      dependency_isolation: true,
      checkpoints: CHAT_COUNT,
      forked: true,
      synced_and_adopted: true,
      published: true,
      daemon_restarts: 1,
      sqlite_inode_preserved: true,
      keepalive: { spawned: true, detached: true, stopped_on_owner_exit: true },
      suspended_and_woken: true,
      heartbeat_leases: uniqueHeartbeatLeases(received),
      events: received.length,
      event_reconnects: 1,
      event_cursor: latestCursor,
      lease_ttl_secs: HARNESS_LEASE_TTL_SECS,
      orphan_grace_secs: HARNESS_ORPHAN_GRACE_SECS,
      gc_rounds: gcRounds,
      doctor,
      cleanup: { ...cleanup, status: "clean" },
    };
  } finally {
    abortEvents.abort();
    await eventTask?.catch(() => undefined);
    await daemon?.stop().catch(() => undefined);
    await npmFixture?.close();
    if (!keepRoot) await rm(harnessRoot, { recursive: true, force: true });
  }
}

async function createGitFixture(repository: string): Promise<void> {
  await mkdir(repository, { recursive: true });
  await git(repository, ["init", "--initial-branch=main"]);
  await git(repository, ["config", "user.name", "Shade Harness"]);
  await git(repository, ["config", "user.email", "shade-harness@example.invalid"]);
  await writeFile(join(repository, "README.md"), "# real Shade harness\n");
  await mkdir(join(repository, "src"), { recursive: true });
  await writeFile(join(repository, "src", "fixture.txt"), "fixture\n");
}

async function npmInstallCommands(root: string): Promise<string[]> {
  const directory = join(root, "runtime/dependency-native/npm/_logs");
  const commands: string[] = [];
  for (const entry of await readDirectory(directory)) {
    if (!entry.isFile() || !entry.name.endsWith("-debug-0.log")) continue;
    const log = await readFile(join(directory, entry.name), "utf8");
    if (!/^\d+ verbose title npm ci(?:\s|$)/m.test(log)) continue;
    const args = log.match(/^\d+ verbose argv (.*)$/m)?.[1];
    assert.ok(args, "npm ci did not record its invocation");
    assert.match(log, /^\d+ verbose exit 0$/m, "npm ci did not finish successfully");
    commands.push(args);
  }
  return commands.sort();
}

async function sourceFetchCount(trace: string, repository: string): Promise<number> {
  const source = await realpath(repository);
  const isSource = (argument: string) => {
    const path = argument.startsWith("file:") ? fileURLToPath(argument) : argument;
    return path === source || path === join(source, ".git");
  };
  return (await readFile(trace, "utf8")).split("\n").filter(Boolean)
    .map((line) => JSON.parse(line) as { event: string; argv?: string[] })
    .filter((event) => event.event === "start" && event.argv?.includes("fetch") && event.argv.some(isSource)).length;
}

function isRetiredSessionError(error: unknown): boolean {
  return isObject(error) && error.code === "SESSION_HANDLE_RETIRED";
}

function uniqueHeartbeatLeases(events: EventEnvelope[]): number {
  return new Set(
    events.filter((event) => event.event === "lease.heartbeat").map((event) => event.resource),
  ).size;
}

async function databaseInventory(root: string): Promise<DatabaseInventory> {
  const database = join(root, "state.sqlite");
  assert.equal(await pathExists(database), true);
  const values = await databaseScalars(root, [
    "SELECT COUNT(*) FROM sessions WHERE state='active';",
    "SELECT COUNT(*) FROM leases WHERE released_at_ms IS NULL;",
    "SELECT COUNT(*) FROM workspaces WHERE state NOT IN ('deleted','deleting');",
    "SELECT COUNT(*) FROM operations WHERE state='running';",
    "SELECT COUNT(*) FROM handoffs WHERE state='pending';",
    "SELECT COUNT(*) FROM reviews WHERE state='pending';",
    "SELECT COUNT(*) FROM dependency_receipts;",
  ]);
  return {
    active_sessions: values[0]!,
    active_leases: values[1]!,
    live_workspaces: values[2]!,
    running_operations: values[3]!,
    pending_handoffs: values[4]!,
    pending_reviews: values[5]!,
    dependency_receipts: values[6]!,
  };
}

async function databaseScalar(root: string, query: string): Promise<number> {
  return (await databaseScalars(root, [query]))[0]!;
}

async function databaseScalars(root: string, queries: string[]): Promise<number[]> {
  const result = await runCommand("/usr/bin/sqlite3", [
    join(root, "state.sqlite"),
    `PRAGMA query_only=ON;\n${queries.join("\n")}`,
  ]);
  const lines = result.stdout.trim().split("\n").filter(Boolean);
  assert.equal(lines.length, queries.length, `invalid SQLite scalar set: ${result.stdout}`);
  return lines.map((line) => {
    const value = Number(line);
    assert.ok(Number.isSafeInteger(value) && value >= 0, `invalid SQLite scalar: ${line}`);
    return value;
  });
}

async function cleanupInventory(root: string): Promise<CleanupInventory> {
  const repositories = await managedRepositoryPaths(root);
  const database = await databaseInventory(root);
  const workspaceEntries = await directoryNames(join(root, "workspaces"));
  const stagingEntries = await directoryNames(join(root, "dependencies", "staging"));
  const artifacts = await artifactPaths(join(root, "dependencies", "artifacts"));
  const mutableDependencyEntries = await dependencyMutableEntries(root);
  const temporaryEntries = await findTemporaryEntries(root);
  return {
    ...database,
    workspace_entries: workspaceEntries.length,
    linked_worktrees: await linkedWorktreeCount(repositories),
    private_refs: (await privateRefs(repositories)).length,
    dependency_staging_entries: stagingEntries.length,
    dependency_artifacts: artifacts.length,
    dependency_mutable_entries: mutableDependencyEntries.length,
    temporary_entries: temporaryEntries.length,
  };
}

async function dependencyMutableEntries(root: string): Promise<string[]> {
  const found = (await directoryNames(join(root, "dependencies", "staging"))).map((name) =>
    join(root, "dependencies", "staging", name),
  );
  const artifactsRoot = join(root, "dependencies", "artifacts");
  for (const provider of await directoryNames(artifactsRoot)) {
    const providerRoot = join(artifactsRoot, provider);
    for (const entry of await readDirectory(providerRoot)) {
      if (entry.isDirectory() && !/^[0-9a-f]{64}$/.test(entry.name)) {
        found.push(join(providerRoot, entry.name));
      }
    }
  }
  return found.sort();
}

async function managedRepositoryPaths(root: string): Promise<string[]> {
  return (await directoryNames(join(root, "repositories"))).map((name) =>
    join(root, "repositories", name),
  );
}

async function privateRefs(repositories: string[]): Promise<string[]> {
  const refs: string[] = [];
  for (const repository of repositories) {
    const result = await runCommand("git", [
      `--git-dir=${repository}`,
      "for-each-ref",
      "--format=%(refname)",
      "refs/shade/",
    ]);
    refs.push(...result.stdout.split("\n").filter(Boolean));
  }
  return refs;
}

async function linkedWorktreeCount(repositories: string[]): Promise<number> {
  let count = 0;
  for (const repository of repositories) {
    const result = await runCommand("git", [
      `--git-dir=${repository}`,
      "worktree",
      "list",
      "--porcelain",
    ]);
    for (const block of result.stdout.trim().split(/\n\n+/).filter(Boolean)) {
      if (!/^bare$/m.test(block)) count += 1;
    }
  }
  return count;
}

async function countBaseTrees(root: string): Promise<number> {
  let count = 0;
  for (const repository of await directoryNames(root)) {
    count += (await directoryNames(join(root, repository))).length;
  }
  return count;
}

async function artifactPaths(root: string): Promise<string[]> {
  const paths: string[] = [];
  for (const provider of await directoryNames(root)) {
    for (const fingerprint of await directoryNames(join(root, provider))) {
      paths.push(join(root, provider, fingerprint));
    }
  }
  return paths;
}

async function findTemporaryEntries(root: string): Promise<string[]> {
  const found: string[] = [];
  async function visit(path: string): Promise<void> {
    for (const entry of await readDirectory(path)) {
      const child = join(path, entry.name);
      if (
        entry.name.startsWith(".shade-git-") ||
        entry.name.startsWith(".shade-apfs-clone-") ||
        entry.name.startsWith(".shade-materialize-") ||
        entry.name.startsWith(".shade-register-") ||
        entry.name.startsWith("promote-") ||
        (entry.name.startsWith(".") && entry.name.endsWith(".staging"))
      ) {
        found.push(child);
      }
      if (entry.isDirectory() && !entry.isSymbolicLink()) await visit(child);
    }
  }
  await visit(root);
  return found;
}

/** Whether a pid still names a running process this user may signal. */
function processIsAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}
