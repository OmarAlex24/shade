/**
 * A real daemon with a park volume attached, and the smallest fixture that
 * makes the parked tier do anything at all.
 *
 * The tier moves the one thing a workspace can always rebuild -- gitignored
 * build output -- to another volume on `sleep` and brings it back on `wake`.
 * Everything it does is therefore optional, and the cases that matter are the
 * ones where the volume is not what the daemon left behind: unplugged, holding
 * a park that describes another tree, or never configured at all. Each of
 * those is a real directory here, removed or rewritten between the sleep and
 * the wake exactly as a person with an external disk would do it.
 *
 * The daemon is the branch's own binary, frozen into the harness root, on an
 * isolated `SHADE_ROOT` and its own socket. Nothing here can reach the
 * installed daemon or the state under `~/Library/Application Support/Shade`.
 */
import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { chmod, copyFile, mkdir, mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import {
  ShadeClient,
  type EventEnvelope,
  type OpenedSessionPayload,
  type SleepResult,
} from "../../sdk-typescript/src/index.ts";
import {
  COMMAND_TIMEOUT_MS,
  DaemonProcess,
  type DoctorResult,
  type GcResult,
  git,
  isObject,
  pathExists,
  resolveShadeBinary,
  runDoctor,
  runGc,
  runShadeLines,
  runShadeOperation,
} from "./real-daemon.ts";

/** Gitignored by the fixture, so a sleep would otherwise discard it. */
export const BUILD_OUTPUT = "build/out.bin";
/**
 * One mebibyte: enough that parking it is a real copy across a real volume,
 * small enough that a suite may do it repeatedly.
 */
export const BUILD_BYTES = 1024 * 1024;
const BUILD_CONTENT = `shade park fixture\n${"x".repeat(BUILD_BYTES - 19)}`;
/** A file Git tracks, which the park must never touch either way. */
export const TRACKED_FILE = "tracked.txt";
export const TRACKED_CONTENT = "base\n";

/** `sleep`, with what the tier did to the build output. */
export interface ParkedSleepResult extends SleepResult {
  parked: boolean;
  parked_bytes: number;
  park_path?: string;
  park_reason?: string;
}

/** `wake`, with what the tier gave back. The session fields are flattened. */
export interface ParkedWakeResult extends OpenedSessionPayload {
  park_restored: boolean;
  park_restored_bytes: number;
  park_reason?: string;
  next?: string;
}

/** The four park counters of one `shade gc` pass, once they are known to be there. */
export interface ParkSweep {
  deleted: number;
  retained: number;
  orphans_removed: number;
  records_dropped: number;
}

export interface ParkHarnessOptions {
  shade_bin?: string;
  /** Start the daemon with `SHADE_PARK_ROOT` set. The tier is off without it. */
  park?: boolean;
  /**
   * `SHADE_PARK_MIN_BYTES`. Zero parks anything at all, which is what a
   * fixture wants: the production default is 256 MiB.
   */
  min_bytes?: number;
  keep_root?: boolean;
}

export class ParkHarness {
  private constructor(
    private readonly daemon: DaemonProcess,
    private readonly client: ShadeClient,
    private readonly keepRoot: boolean,
    /** Everything this harness owns, including the frozen binary. */
    readonly root: string,
    /** The daemon's `SHADE_ROOT`. */
    readonly state: string,
    readonly socket: string,
    /** The configured park volume, whether or not it is there right now. */
    readonly parkRoot: string,
    readonly repository: string,
    readonly binary: string,
  ) {}

  static async start(options: ParkHarnessOptions = {}): Promise<ParkHarness> {
    const sourceBinary = await resolveShadeBinary(options.shade_bin);
    assert.equal(process.platform, "darwin", "the park harness requires macOS");
    const root = await realpath(await mkdtemp(join(tmpdir(), "shade-park-harness-")));
    const state = join(root, "state");
    const socket = join(state, "shade.sock");
    const parkRoot = join(root, "park");
    const repository = join(root, "fixture");
    const binary = join(root, "shade");
    const parked = options.park ?? true;

    await copyFile(sourceBinary, binary);
    await chmod(binary, 0o500);
    await createParkFixture(repository);
    // The daemon never creates the park root: a volume that has to be made is
    // a directory on the boot disk, which is the one place the tier exists to
    // keep these bytes off. An absent one is simply unmounted.
    if (parked) await mkdir(parkRoot, { recursive: true });
    const daemon = await DaemonProcess.start(binary, state, socket, {
      // Blank rather than unset for the unconfigured case: the daemon inherits
      // this process's environment, and a developer who parks their own work
      // has `SHADE_PARK_ROOT` exported. A blank value is what the config reads
      // as "no volume", so the case proves what it claims either way.
      env: parked
        ? {
          SHADE_PARK_ROOT: parkRoot,
          SHADE_PARK_MIN_BYTES: String(options.min_bytes ?? 0),
        }
        : { SHADE_PARK_ROOT: "", SHADE_PARK_MIN_BYTES: "" },
    });
    const client = new ShadeClient({
      socket,
      actor: { kind: "host", id: `park-harness-${randomUUID().slice(0, 8)}` },
      timeout_ms: COMMAND_TIMEOUT_MS,
      operation_poll_ms: 10,
    });
    return new ParkHarness(
      daemon,
      client,
      options.keep_root ?? process.env.SHADE_HARNESS_KEEP_ROOT === "1",
      root,
      state,
      socket,
      parkRoot,
      repository,
      binary,
    );
  }

  /** Open a session on the fixture, with no keepalive to outlive the command. */
  async open(session: string): Promise<OpenedSessionPayload> {
    const opened = await this.run<OpenedSessionPayload>([
      "open",
      this.repository,
      "--session",
      session,
      "--base",
      "main",
      "--no-keepalive",
    ], `open:${session}`);
    assert.equal(opened.session, session);
    return opened;
  }

  async sleep(session: string, workspace: string): Promise<ParkedSleepResult> {
    return await this.run<ParkedSleepResult>(
      ["sleep", "--workspace", workspace],
      `sleep:${session}`,
    );
  }

  async wake(session: string): Promise<ParkedWakeResult> {
    return await this.run<ParkedWakeResult>(
      ["wake", "--session", session, "--no-keepalive"],
      `wake:${session}`,
    );
  }

  async doctor(): Promise<DoctorResult> {
    return await runDoctor(this.binary, this.state, this.socket);
  }

  async gc(key: string): Promise<GcResult> {
    return await runGc(this.binary, this.state, this.socket, this.client, `gc:${key}`);
  }

  /** Every event the daemon has recorded, as `shade events` prints them. */
  async events(): Promise<EventEnvelope[]> {
    const lines = await runShadeLines(this.binary, this.state, this.socket, [
      "events",
      "--after",
      "0",
      "--limit",
      "1000",
    ]);
    return lines.map((line) => {
      const parsed: unknown = JSON.parse(line);
      assert.ok(isObject(parsed) && typeof parsed.event === "string", `not an event: ${line}`);
      return parsed as unknown as EventEnvelope;
    });
  }

  /** `<park root>/<workspace>/<checkpoint>`, the layout an operator can walk. */
  parkDir(slept: ParkedSleepResult): string {
    return join(this.parkRoot, slept.workspace, slept.checkpoint_id);
  }

  /** The build output a workspace would otherwise lose to a sleep. */
  async writeBuildOutput(cwd: string): Promise<void> {
    const path = join(cwd, BUILD_OUTPUT);
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, BUILD_CONTENT);
  }

  /** The disk goes home in someone's bag. */
  async unplugPark(): Promise<void> {
    await rm(this.parkRoot, { recursive: true, force: true });
    assert.equal(await pathExists(this.parkRoot), false);
  }

  /** It comes back, with whatever is still on it. */
  async plugInPark(): Promise<void> {
    await mkdir(this.parkRoot, { recursive: true });
  }

  /** What Git makes of a workspace: empty means the tree is exactly the base. */
  async gitStatus(cwd: string): Promise<string> {
    return (await git(cwd, ["status", "--porcelain"])).stdout.trim();
  }

  async close(): Promise<void> {
    await this.daemon.stop().catch(() => undefined);
    if (!this.keepRoot) await rm(this.root, { recursive: true, force: true });
  }

  /**
   * One CLI command, under an idempotency key no other call can collide with.
   * A stable key would be more faithful to how a host retries, and would also
   * mean a case that slept the same session twice silently got the first
   * sleep's answer back from the cache -- a green test for an operation that
   * never ran.
   */
  private async run<T>(args: string[], key: string): Promise<T> {
    return await runShadeOperation<T>(this.binary, this.state, this.socket, this.client, [
      "--idempotency-key",
      `${key}:${randomUUID().slice(0, 8)}`,
      ...args,
    ]);
  }
}

/**
 * The counters a daemon that has the parked tier always reports. Reading them
 * through here is the assertion that they are there at all: a `gc` that
 * answers without them is a daemon from before the tier, not a quiet zero.
 */
export function parkSweep(result: GcResult): ParkSweep {
  const counters = {
    deleted: result.parks_deleted,
    retained: result.parks_retained,
    orphans_removed: result.parks_orphans_removed,
    records_dropped: result.park_records_dropped,
  };
  for (const [name, value] of Object.entries(counters)) {
    assert.equal(typeof value, "number", `gc did not report parks_${name}: ${JSON.stringify(result)}`);
  }
  return counters as ParkSweep;
}

/** The parked build output, by content rather than by size. */
export function buildOutputDigest(): string {
  return createHash("sha256").update(BUILD_CONTENT).digest("hex");
}

export async function fileDigest(path: string): Promise<string> {
  return createHash("sha256").update(await readFile(path)).digest("hex");
}

/**
 * A repository whose `.gitignore` claims `build/`, so everything written there
 * is private to the workspace and invisible to every checkpoint. That is the
 * only precondition the tier has.
 */
async function createParkFixture(repository: string): Promise<void> {
  await mkdir(repository, { recursive: true });
  await git(repository, ["init", "--initial-branch=main"]);
  await git(repository, ["config", "user.name", "Shade Harness"]);
  await git(repository, ["config", "user.email", "shade-harness@example.invalid"]);
  await writeFile(join(repository, TRACKED_FILE), TRACKED_CONTENT);
  await writeFile(join(repository, ".gitignore"), "build/\n");
  await git(repository, ["add", "--all"]);
  await git(repository, ["commit", "-m", "park fixture"]);
}
