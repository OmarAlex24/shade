/**
 * The parked tier, driven the way a host drives it: one real daemon per case,
 * on its own `SHADE_ROOT`, answering `shade sleep`, `shade wake`, `shade
 * events`, `shade doctor` and `shade gc` in JSON.
 *
 * `crates/shade-engine/tests/park.rs` proves the same invariants against the
 * engine in-process. These cases exist because a host never sees the engine:
 * it sees a CLI on a socket, a directory that must contain its build output
 * afterwards, and a `git status` that must be as clean as it was before. The
 * failures the tier is designed around -- an unplugged volume, a park that
 * describes another tree, a daemon with no volume configured -- are real
 * directories here, removed or rewritten between the sleep and the wake.
 */
import { afterEach, describe, expect, test } from "bun:test";
import { readFile, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";

import {
  BUILD_BYTES,
  BUILD_OUTPUT,
  ParkHarness,
  TRACKED_CONTENT,
  TRACKED_FILE,
  buildOutputDigest,
  fileDigest,
  parkSweep,
  type ParkedSleepResult,
} from "../src/park-harness.ts";
import { pathExists } from "../src/real-daemon.ts";

/** The same gate the real harness uses: no built binary, no real daemon. */
const parkTest = process.env.SHADE_BIN === undefined ? test.skip : test;
const CASE_TIMEOUT_MS = 180_000;

let harness: ParkHarness | undefined;

afterEach(async () => {
  await harness?.close();
  harness = undefined;
});

describe("parked tier over the CLI", () => {
  parkTest(
    "parks the build output on sleep and puts it back on wake",
    async () => {
      harness = await ParkHarness.start();
      const opened = await harness.open("park-present");
      await harness.writeBuildOutput(opened.cwd);

      const slept = await harness.sleep("park-present", opened.workspace);
      expect(slept.suspended).toBe(true);
      expect(slept.parked).toBe(true);
      expect(slept.park_reason).toBeUndefined();
      expect(slept.parked_bytes).toBe(BUILD_BYTES);
      expect(await pathExists(opened.cwd)).toBe(false);

      // The layout an operator has to be able to walk by hand:
      // `<root>/<workspace>/<checkpoint>/{manifest.json,tree/}`.
      const park = harness.parkDir(slept);
      expect(slept.park_path).toBe(park);
      expect((await stat(join(park, "manifest.json"))).isFile()).toBe(true);
      const manifest = await readManifest(harness, slept);
      expect(manifest.workspace_id).toBe(slept.workspace);
      expect(manifest.checkpoint_id).toBe(slept.checkpoint_id);
      expect(manifest.bytes).toBe(BUILD_BYTES);
      expect(await fileDigest(join(park, "tree", BUILD_OUTPUT))).toBe(buildOutputDigest());

      // Parking announces itself, so a host that never asked for the sleep
      // still learns the bytes are somewhere.
      const parkedEvent = (await harness.events()).find(
        (event) => event.event === "workspace.parked",
      );
      expect(parkedEvent).toBeDefined();
      expect(parkedEvent?.resource).toBe(slept.workspace);
      expect(parkedEvent?.payload).toMatchObject({
        checkpoint_id: slept.checkpoint_id,
        bytes: BUILD_BYTES,
        park_path: park,
      });

      const suspended = await harness.doctor();
      // The daemon answering is this harness's own, on its own root: no case
      // here can reach an installed daemon or the state it owns.
      expect(suspended.root).toBe(harness.state);
      expect(suspended.park_root).toBe(harness.parkRoot);
      expect(suspended.park_mounted).toBe(true);
      expect(suspended.parks).toBe(1);
      expect(suspended.park_bytes).toBe(BUILD_BYTES);
      expect(suspended.parks_orphaned).toBe(0);

      const woken = await harness.wake("park-present");
      expect(woken.park_restored).toBe(true);
      expect(woken.park_restored_bytes).toBe(BUILD_BYTES);
      expect(woken.park_reason).toBeUndefined();
      expect(woken.next).toBeUndefined();
      expect(woken.workspace).not.toBe(opened.workspace);

      // The successor is the tree the agent left: the same bytes, byte for
      // byte, and a working tree Git still considers untouched.
      expect(await fileDigest(join(woken.cwd, BUILD_OUTPUT))).toBe(buildOutputDigest());
      expect(await readFile(join(woken.cwd, TRACKED_FILE), "utf8")).toBe(TRACKED_CONTENT);
      expect(await harness.gitStatus(woken.cwd)).toBe("");

      // A park is spent by the wake that restores it.
      expect(await pathExists(park)).toBe(false);
      expect((await harness.doctor()).parks).toBe(0);
    },
    CASE_TIMEOUT_MS,
  );

  parkTest(
    "wakes without the build output when the volume went home in a bag",
    async () => {
      harness = await ParkHarness.start();
      const opened = await harness.open("park-unplugged");
      await harness.writeBuildOutput(opened.cwd);
      const slept = await harness.sleep("park-unplugged", opened.workspace);
      expect(slept.parked).toBe(true);

      await harness.unplugPark();

      const woken = await harness.wake("park-unplugged");
      expect(woken.park_restored).toBe(false);
      expect(woken.park_restored_bytes).toBe(0);
      expect(woken.park_reason).toBe("unmounted");
      // The one case that earns a sentence: the successor is correct and will
      // still take a full build.
      expect(woken.next).toContain("unmounted");

      // Exactly the wake it was before the tier existed.
      expect(await pathExists(join(woken.cwd, BUILD_OUTPUT))).toBe(false);
      expect(await readFile(join(woken.cwd, TRACKED_FILE), "utf8")).toBe(TRACKED_CONTENT);
      expect(await harness.gitStatus(woken.cwd)).toBe("");

      // An absent disk is not evidence that the bytes are gone, so the record
      // outlives the wake that could not use it.
      const doctor = await harness.doctor();
      expect(doctor.park_mounted).toBe(false);
      expect(doctor.park_root).toBe(harness.parkRoot);
      expect(doctor.parks).toBe(1);
    },
    CASE_TIMEOUT_MS,
  );

  parkTest(
    "declines a park whose manifest describes another tree",
    async () => {
      harness = await ParkHarness.start();
      const opened = await harness.open("park-mismatch");
      await harness.writeBuildOutput(opened.cwd);
      const slept = await harness.sleep("park-mismatch", opened.workspace);
      expect(slept.parked).toBe(true);

      // The bytes on the volume are fine; what the manifest claims about them
      // is not. Restoring them anyway would put another checkpoint's build
      // output into this successor.
      const manifest = await readManifest(harness, slept);
      manifest.worktree_oid = "0".repeat(40);
      await writeManifest(harness, slept, manifest);

      const woken = await harness.wake("park-mismatch");
      expect(woken.park_restored).toBe(false);
      expect(woken.park_restored_bytes).toBe(0);
      expect(woken.park_reason).toBe("manifest_mismatch");
      expect(woken.next).toContain("manifest_mismatch");

      expect(await pathExists(woken.cwd)).toBe(true);
      expect(await pathExists(join(woken.cwd, BUILD_OUTPUT))).toBe(false);
      expect(await readFile(join(woken.cwd, TRACKED_FILE), "utf8")).toBe(TRACKED_CONTENT);
      expect(await harness.gitStatus(woken.cwd)).toBe("");
    },
    CASE_TIMEOUT_MS,
  );

  parkTest(
    "declines a park whose manifest does not parse, as an absent one",
    async () => {
      harness = await ParkHarness.start();
      const opened = await harness.open("park-corrupt");
      await harness.writeBuildOutput(opened.cwd);
      const slept = await harness.sleep("park-corrupt", opened.workspace);
      expect(slept.parked).toBe(true);

      // Half a manifest is what an interrupted copy or a full volume leaves.
      // The park is described by nothing readable, which the daemon cannot
      // tell apart from no manifest at all -- so it says `absent`, not
      // `manifest_mismatch`, and that distinction is the contract a host sees.
      await writeFile(join(harness.parkDir(slept), "manifest.json"), '{"version":');

      const woken = await harness.wake("park-corrupt");
      expect(woken.park_restored).toBe(false);
      expect(woken.park_reason).toBe("absent");
      expect(woken.next).toContain("absent");
      expect(await pathExists(join(woken.cwd, BUILD_OUTPUT))).toBe(false);
      expect(await harness.gitStatus(woken.cwd)).toBe("");
    },
    CASE_TIMEOUT_MS,
  );

  parkTest(
    "says the tier is off rather than broken when no volume is configured",
    async () => {
      harness = await ParkHarness.start({ park: false });
      const opened = await harness.open("park-unconfigured");
      await harness.writeBuildOutput(opened.cwd);

      const slept = await harness.sleep("park-unconfigured", opened.workspace);
      expect(slept.suspended).toBe(true);
      expect(slept.parked).toBe(false);
      expect(slept.parked_bytes).toBe(0);
      expect(slept.park_path).toBeUndefined();
      expect(slept.park_reason).toBe("unconfigured");

      const doctor = await harness.doctor();
      expect(doctor.park_root).toBeNull();
      expect(doctor.park_mounted).toBe(false);
      expect(doctor.parks).toBe(0);
      expect(doctor.park_bytes).toBe(0);
      expect(doctor.parks_orphaned).toBe(0);
      expect((await harness.events()).some((event) => event.event === "workspace.parked")).toBe(
        false,
      );

      // Nothing was parked, so the wake lost nothing and says nothing.
      const woken = await harness.wake("park-unconfigured");
      expect(woken.park_restored).toBe(false);
      expect(woken.park_reason).toBeUndefined();
      expect(woken.next).toBeUndefined();
      expect(await pathExists(join(woken.cwd, BUILD_OUTPUT))).toBe(false);
      expect(await harness.gitStatus(woken.cwd)).toBe("");
    },
    CASE_TIMEOUT_MS,
  );

  parkTest(
    "discards build output too small to be worth the copy",
    async () => {
      // A mounted volume the daemon may not use: `SHADE_PARK_MIN_BYTES` is the
      // gate that keeps a slow external copy from being spent on a megabyte,
      // and nothing this fixture writes can clear a bar of a gibibyte.
      harness = await ParkHarness.start({ min_bytes: 1024 * 1024 * 1024 });
      const opened = await harness.open("park-too-small");
      await harness.writeBuildOutput(opened.cwd);

      const slept = await harness.sleep("park-too-small", opened.workspace);
      expect(slept.suspended).toBe(true);
      expect(slept.parked).toBe(false);
      expect(slept.parked_bytes).toBe(0);
      expect(slept.park_reason).toBe("below_min_bytes");

      const doctor = await harness.doctor();
      expect(doctor.park_mounted).toBe(true);
      expect(doctor.parks).toBe(0);
      expect(await pathExists(join(harness.parkRoot, slept.workspace))).toBe(false);
    },
    CASE_TIMEOUT_MS,
  );

  parkTest(
    "collects a stranded park only once the volume is back",
    async () => {
      harness = await ParkHarness.start();
      const opened = await harness.open("park-gc");
      await harness.writeBuildOutput(opened.cwd);
      const slept = await harness.sleep("park-gc", opened.workspace);
      expect(slept.parked).toBe(true);
      await harness.unplugPark();
      expect((await harness.wake("park-gc")).park_reason).toBe("unmounted");

      // The record now describes a checkpoint that stopped being a suspension,
      // which is garbage -- but the collector may not act on garbage it cannot
      // see, or the bytes would be stranded with nothing left knowing their
      // name.
      const unmounted = parkSweep(await harness.gc("unmounted"));
      expect(unmounted.retained).toBeGreaterThanOrEqual(1);
      expect(unmounted.deleted).toBe(0);
      expect(unmounted.records_dropped).toBe(0);
      const stranded = await harness.doctor();
      expect(stranded.park_mounted).toBe(false);
      expect(stranded.parks).toBe(1);

      // Plugged back in, the same verdict can finally be carried out. Whether
      // the pass removes a directory or drops a record that has none is the
      // volume's business; the daemon must stop claiming those bytes either
      // way.
      await harness.plugInPark();
      const mounted = parkSweep(await harness.gc("mounted"));
      expect(mounted.deleted + mounted.records_dropped).toBeGreaterThanOrEqual(1);
      const collected = await harness.doctor();
      expect(collected.park_mounted).toBe(true);
      expect(collected.parks).toBe(0);
      expect(collected.park_bytes).toBe(0);
      expect(collected.parks_orphaned).toBe(0);
    },
    CASE_TIMEOUT_MS,
  );
});

interface ParkManifest {
  version: number;
  workspace_id: string;
  checkpoint_id: string;
  head_oid: string;
  worktree_oid: string;
  bytes: number;
  entries: { path: string; bytes: number }[];
}

async function readManifest(
  harness: ParkHarness,
  slept: ParkedSleepResult,
): Promise<ParkManifest> {
  return JSON.parse(
    await readFile(join(harness.parkDir(slept), "manifest.json"), "utf8"),
  ) as ParkManifest;
}

async function writeManifest(
  harness: ParkHarness,
  slept: ParkedSleepResult,
  manifest: ParkManifest,
): Promise<void> {
  await writeFile(join(harness.parkDir(slept), "manifest.json"), JSON.stringify(manifest));
}
