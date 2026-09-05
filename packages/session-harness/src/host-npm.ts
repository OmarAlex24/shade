import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { mkdir, readFile, realpath, writeFile } from "node:fs/promises";
import { join } from "node:path";

type Runner = (command: string, args: string[], options?: { env?: NodeJS.ProcessEnv }) => Promise<{ stdout: string; stderr: string }>;

export interface HostToolEvidence {
  name: "npm" | "node";
  version: string;
  digest: string;
  path_identity: string;
}

export interface NpmFixture {
  tools: HostToolEvidence[];
  tarball_requests: () => number[];
  close: () => Promise<void>;
}

export async function createHostNpmFixture(repository: string, root: string, run: Runner): Promise<NpmFixture> {
  const hostHome = join(root, "host-home");
  await mkdir(hostHome, { recursive: true });
  const env = { PATH: process.env.PATH ?? "/usr/bin:/bin", HOME: hostHome, CI: "1", NPM_CONFIG_IGNORE_SCRIPTS: "true" };
  const npm = await realpath((await run("/usr/bin/which", ["npm"])).stdout.trim());
  const node = await realpath((await run("/usr/bin/which", ["node"])).stdout.trim());
  const npmVersion = (await run(node, [npm, "--version"], { env })).stdout.trim();
  const nodeVersion = (await run(node, ["--version"], { env })).stdout.trim();
  assert.match(npmVersion, /^\d+\.\d+\.\d+(?:[-+].*)?$/, "an installed npm is required");
  const tools: HostToolEvidence[] = await Promise.all(
    ([{ name: "npm", path: npm, version: npmVersion }, { name: "node", path: node, version: nodeVersion }] as const).map(async (tool) => {
      const digest = createHash("sha256").update(await readFile(tool.path)).digest("hex");
      return { name: tool.name, version: tool.version, digest, path_identity: `${tool.name}@sha256:${digest}` };
    }),
  );
  const archives: Buffer[] = [];
  for (const major of [1, 2]) {
    const packageRoot = join(root, `registry-${major}`, "package");
    await mkdir(packageRoot, { recursive: true });
    await writeFile(join(packageRoot, "package.json"), JSON.stringify({ name: "shade-fixture", version: `${major}.0.0`, type: "module", scripts: { preinstall: "exit 93", install: "exit 94", postinstall: "exit 95" } }));
    await writeFile(join(packageRoot, "index.js"), major === 1 ? "export const shadeFixture = true;\n" : "export const shadeFixture = 2;\n");
    const archive = join(root, `fixture-${major}.tgz`);
    await run("/usr/bin/tar", ["-czf", archive, "-C", join(root, `registry-${major}`), "package"], { env: { ...env, COPYFILE_DISABLE: "1" } });
    archives.push(await readFile(archive));
  }
  const requests = [0, 0];
  const server = createServer((request, response) => {
    const index = ["/fixture-1.tgz", "/fixture-2.tgz"].indexOf(request.url ?? "");
    const body = archives[index];
    if (body === undefined) { response.writeHead(404); response.end("{}"); return; }
    requests[index] = requests[index]! + 1;
    response.writeHead(200, { "content-type": "application/octet-stream", "content-length": body.length });
    response.end(body);
  });
  await new Promise<void>((resolve, reject) => { server.once("error", reject); server.listen(0, "127.0.0.1", resolve); });
  const address = server.address();
  assert.ok(address !== null && typeof address !== "string");
  const url = `http://127.0.0.1:${address.port}`;
  try {
    await mkdir(join(repository, "packages", "member"), { recursive: true });
    await writeFile(join(repository, ".npmrc"), `registry=${url}/\n`);
    const rootManifest = { name: "shade-real-harness", version: "1.0.0", private: true, packageManager: `npm@${npmVersion}`, workspaces: ["packages/*"], dependencies: { "shade-fixture": "1.0.0" }, scripts: { preinstall: "exit 91", postinstall: "exit 92" } };
    const memberManifest = { name: "shade-member", version: "1.0.0", private: true, dependencies: { "shade-fixture": "2.0.0" }, scripts: { postinstall: "exit 96" } };
    await writeFile(join(repository, "package.json"), `${JSON.stringify(rootManifest)}\n`);
    await writeFile(join(repository, "packages", "member", "package.json"), `${JSON.stringify(memberManifest)}\n`);
    const lockedPackage = (major: number) => ({ version: `${major}.0.0`, resolved: `${url}/fixture-${major}.tgz`, integrity: `sha512-${createHash("sha512").update(archives[major - 1]!).digest("base64")}`, hasInstallScript: true });
    await writeFile(join(repository, "package-lock.json"), `${JSON.stringify({ name: rootManifest.name, version: "1.0.0", lockfileVersion: 3, requires: true, packages: {
      "": rootManifest,
      "node_modules/shade-fixture": lockedPackage(1),
      "node_modules/shade-member": { resolved: "packages/member", link: true },
      "packages/member": memberManifest,
      "packages/member/node_modules/shade-fixture": lockedPackage(2),
    } })}\n`);
  } catch (error) { await close(server); throw error; }
  return { tools, tarball_requests: () => [...requests], close: () => close(server) };
}

async function close(server: Server): Promise<void> {
  await new Promise<void>((resolve, reject) => { server.close((error) => error ? reject(error) : resolve()); server.closeAllConnections(); });
}
