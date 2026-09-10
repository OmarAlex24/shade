/**
 * Driving a real `shade` daemon from a test: the process, the CLI and the
 * envelope every command answers in.
 *
 * The counterpart of `fake-daemon.ts`. Everything here is the machinery a
 * harness needs before it can assert anything -- a daemon on an isolated
 * `SHADE_ROOT`, one JSON record per command, and the durable-operation
 * recovery a CLI exit of 75 asks for -- and nothing here knows what any
 * particular harness is trying to prove.
 */
import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import { constants as fsConstants } from "node:fs";
import { access, lstat, mkdir, readdir, realpath } from "node:fs/promises";
import { createConnection } from "node:net";
import { isAbsolute, resolve } from "node:path";

import type { ShadeClient, TerminalOutcome } from "../../sdk-typescript/src/index.ts";

export const COMMAND_TIMEOUT_MS = 120_000;
/**
 * The hidden, explicit lifecycle override every harness daemon runs with. It
 * exercises the production GC predicates without editing SQLite or waiting out
 * the fixed 120-second lease TTL and 600-second orphan grace of an ordinary
 * daemon.
 */
export const HARNESS_LEASE_TTL_SECS = 120;
export const HARNESS_ORPHAN_GRACE_SECS = 0;

export interface CommandResult {
  stdout: string;
  stderr: string;
}

export interface DoctorResult {
  state: string;
  sessions: number;
  workspaces: number;
  operations: number;
  sessions_dormant: number;
  sessions_suspended: number;
  workspaces_suspended: number;
  suspended_without_checkpoint: number;
  protocol: number;
  root: string;
  /** The parked tier, as the daemon sees it without touching the volume. */
  park_root: string | null;
  park_mounted: boolean;
  parks: number;
  park_bytes: number;
  parks_orphaned: number;
}

export interface GcResult {
  eligible: number;
  deleted: number;
  skipped: number;
  layers_deleted: number;
  layers_reclaimed: number;
  /**
   * The parked tier's four counters, optional for the same reason the wire
   * types default them: a daemon built before the tier answers a `gc` without
   * any of them, and a harness that reads one must say so rather than assume a
   * zero. `parkSweep` in `park-harness.ts` is where they stop being optional.
   */
  /** Garbage parks taken off a mounted volume, with their records. */
  parks_deleted?: number;
  /** Garbage parks left alone because the volume is not mounted. */
  parks_retained?: number;
  /** Park directories on the volume that no record claims. */
  parks_orphans_removed?: number;
  /** Records whose directory is gone from a mounted volume. */
  park_records_dropped?: number;
}

/** A `shade daemon` child on its own state root and socket. */
export class DaemonProcess {
  private output = "";

  private constructor(
    private readonly child: ChildProcess,
    readonly socket: string,
  ) {
    child.stdout?.setEncoding("utf8");
    child.stderr?.setEncoding("utf8");
    child.stdout?.on("data", (chunk: string) => this.capture(chunk));
    child.stderr?.on("data", (chunk: string) => this.capture(chunk));
  }

  /**
   * `env` is merged over the daemon's own environment, which is how a harness
   * configures anything the daemon reads at startup: the Git trace, the park
   * volume. `SHADE_ROOT` is always the isolated root, never the caller's.
   */
  static async start(
    binary: string,
    root: string,
    socket: string,
    options: { env?: NodeJS.ProcessEnv } = {},
  ): Promise<DaemonProcess> {
    await mkdir(root, { recursive: true });
    const child = spawn(binary, [
      "--socket",
      socket,
      "daemon",
      "--harness-lifecycle",
      "--harness-lease-ttl-secs",
      String(HARNESS_LEASE_TTL_SECS),
      "--harness-orphan-grace-secs",
      String(HARNESS_ORPHAN_GRACE_SECS),
    ], {
      env: {
        ...process.env,
        ...options.env,
        SHADE_ROOT: root,
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const daemon = new DaemonProcess(child, socket);
    let spawnError: unknown;
    child.once("error", (error) => {
      spawnError = error;
    });
    await waitUntil(
      async () => {
        if (spawnError !== undefined) throw spawnError;
        if (child.exitCode !== null || child.signalCode !== null) {
          throw new Error(`shade daemon exited before readiness: ${daemon.output}`);
        }
        return await socketAcceptsConnections(socket);
      },
      10_000,
      "shade daemon readiness",
    );
    return daemon;
  }

  async crash(): Promise<void> {
    await this.terminate("SIGKILL");
  }

  async stop(): Promise<void> {
    await this.terminate("SIGTERM");
  }

  private async terminate(signal: NodeJS.Signals): Promise<void> {
    if (this.child.exitCode !== null || this.child.signalCode !== null) return;
    this.child.kill(signal);
    if (!(await waitForExit(this.child, 5_000))) {
      this.child.kill("SIGKILL");
      assert.equal(await waitForExit(this.child, 5_000), true, "daemon would not exit");
    }
  }

  private capture(chunk: string): void {
    this.output = `${this.output}${chunk}`.slice(-32_768);
  }
}

export async function runDoctor(
  binary: string,
  root: string,
  socket: string,
): Promise<DoctorResult> {
  const envelope = await runShade(binary, root, socket, ["doctor"]);
  return completedEnvelope<DoctorResult>(envelope);
}

export async function runGc(
  binary: string,
  root: string,
  socket: string,
  client: ShadeClient,
  idempotencyKey: string,
): Promise<GcResult> {
  return await runShadeOperation<GcResult>(binary, root, socket, client, [
    "--idempotency-key",
    idempotencyKey,
    "gc",
  ]);
}

/**
 * Run one CLI command that carries an operation, and return its completed
 * result however the daemon chose to answer: inline, as an accepted handle, or
 * as the exit-75 recovery envelope that still names the durable operation.
 */
export async function runShadeOperation<T>(
  binary: string,
  root: string,
  socket: string,
  client: ShadeClient,
  args: string[],
): Promise<T> {
  const envelope = await runShade(binary, root, socket, args);
  if (envelope.status === "error" && isObject(envelope.error)
    && envelope.error.code === "CLIENT_TIMEOUT" && typeof envelope.error.operation === "string") {
    return expectCompleted(
      await client.operations.wait<T>(envelope.error.operation, {
        timeout_ms: COMMAND_TIMEOUT_MS,
      }),
    );
  }
  const outcome = okOutcome(envelope);
  if (outcome.state === "accepted") {
    const operationId = outcome.result.operation_id;
    if (typeof operationId !== "string") {
      throw new Error("accepted CLI operation has no operation_id");
    }
    return expectCompleted(
      await client.operations.wait<T>(operationId, { timeout_ms: COMMAND_TIMEOUT_MS }),
    );
  }
  assert.equal(outcome.state, "completed", "CLI returned a non-completed domain outcome");
  return outcome.result as unknown as T;
}

export async function runShade(
  binary: string,
  root: string,
  socket: string,
  args: string[],
): Promise<Record<string, unknown>> {
  const lines = await runShadeLines(binary, root, socket, args);
  assert.equal(lines.length, 1, "shade CLI must emit exactly one JSON record");
  const parsed: unknown = JSON.parse(lines[0]!);
  assert.ok(isObject(parsed), "shade CLI response must be an object");
  return parsed;
}

/**
 * The same command, for the pages the CLI prints as one JSON value per line:
 * `shade events` is a page of envelopes, not a single record.
 */
export async function runShadeLines(
  binary: string,
  root: string,
  socket: string,
  args: string[],
): Promise<string[]> {
  const result = await runCommand(binary, ["--socket", socket, ...args], {
    env: { ...process.env, SHADE_ROOT: root },
    // Exit 75 carries a valid recovery envelope, including the durable handle.
    // Callers must inspect it instead of discarding stdout as a command error.
    allowed_exit_codes: [75],
  });
  return result.stdout.trim().split("\n").filter(Boolean);
}

export function completedEnvelope<T>(envelope: Record<string, unknown>): T {
  const outcome = okOutcome(envelope);
  assert.equal(outcome.state, "completed", "expected completed CLI response");
  return outcome.result as T;
}

export function okOutcome(envelope: Record<string, unknown>): {
  state: string;
  result: Record<string, unknown>;
} {
  assert.equal(envelope.status, "ok", JSON.stringify(envelope));
  assert.ok(isObject(envelope.outcome), "missing CLI outcome");
  assert.equal(typeof envelope.outcome.state, "string");
  assert.ok(isObject(envelope.outcome.result), "missing CLI outcome result");
  return envelope.outcome as { state: string; result: Record<string, unknown> };
}

export function expectCompleted<T>(outcome: TerminalOutcome<T>): T {
  assert.equal(outcome.state, "completed", JSON.stringify(outcome));
  return (outcome as Extract<TerminalOutcome<T>, { state: "completed" }>).result;
}

export async function git(repository: string, args: string[]): Promise<CommandResult> {
  return await runCommand("git", ["-C", repository, ...args]);
}

export async function resolveShadeBinary(configured?: string): Promise<string> {
  const input = configured ?? process.env.SHADE_BIN;
  assert.ok(input, "SHADE_BIN must point to a built shade binary");
  const absolute = isAbsolute(input) ? input : resolve(process.cwd(), input);
  await access(absolute, fsConstants.X_OK);
  return await realpath(absolute);
}

export async function runCommand(
  command: string,
  args: string[],
  options: { env?: NodeJS.ProcessEnv; timeout_ms?: number; allowed_exit_codes?: readonly number[] } = {},
): Promise<CommandResult> {
  return await new Promise((resolvePromise, rejectPromise) => {
    const child = spawn(command, args, {
      env: options.env ?? process.env,
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    let settled = false;
    const timeout = setTimeout(() => {
      child.kill("SIGKILL");
      finish(new Error(`command timed out: ${command}`));
    }, options.timeout_ms ?? COMMAND_TIMEOUT_MS);
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      stdout = `${stdout}${chunk}`.slice(-4 * 1024 * 1024);
    });
    child.stderr.on("data", (chunk: string) => {
      stderr = `${stderr}${chunk}`.slice(-4 * 1024 * 1024);
    });
    child.once("error", (error) => finish(error));
    child.once("close", (code, signal) => {
      if (code === 0 || (code !== null && options.allowed_exit_codes?.includes(code))) finish(undefined, { stdout, stderr });
      else {
        finish(
          new Error(
            `command failed (${code ?? signal ?? "unknown"}): ${command} ${args.join(" ")}\n${stderr}`,
          ),
        );
      }
    });

    function finish(error?: Error, result?: CommandResult): void {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      if (error !== undefined) rejectPromise(error);
      else resolvePromise(result!);
    }
  });
}

export async function socketAcceptsConnections(path: string): Promise<boolean> {
  return await new Promise((resolvePromise) => {
    const socket = createConnection({ path });
    const done = (connected: boolean) => {
      socket.removeAllListeners();
      socket.destroy();
      resolvePromise(connected);
    };
    socket.once("connect", () => done(true));
    socket.once("error", () => done(false));
  });
}

export async function waitForExit(child: ChildProcess, timeoutMs: number): Promise<boolean> {
  if (child.exitCode !== null || child.signalCode !== null) return true;
  return await new Promise((resolvePromise) => {
    const timeout = setTimeout(() => finish(false), timeoutMs);
    child.once("exit", () => finish(true));
    function finish(exited: boolean): void {
      clearTimeout(timeout);
      child.removeAllListeners("exit");
      resolvePromise(exited);
    }
  });
}

export async function waitUntil(
  predicate: () => boolean | Promise<boolean>,
  timeoutMs: number,
  description: string,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!(await predicate())) {
    if (Date.now() >= deadline) throw new Error(`timed out waiting for ${description}`);
    await sleep(20);
  }
}

export async function sleep(ms: number): Promise<void> {
  await new Promise((resolvePromise) => setTimeout(resolvePromise, ms));
}

export async function directoryNames(path: string): Promise<string[]> {
  return (await readDirectory(path))
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
    .sort();
}

export async function readDirectory(path: string) {
  try {
    return await readdir(path, { withFileTypes: true });
  } catch (error) {
    if (isNodeError(error) && error.code === "ENOENT") return [];
    throw error;
  }
}

export async function pathExists(path: string): Promise<boolean> {
  try {
    await lstat(path);
    return true;
  } catch (error) {
    if (isNodeError(error) && error.code === "ENOENT") return false;
    throw error;
  }
}

export function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function isNodeError(error: unknown): error is NodeJS.ErrnoException {
  return error instanceof Error && "code" in error;
}
