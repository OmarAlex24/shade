import { expect, test } from "bun:test";
import { ShadeClient, type OpenedSessionPayload, type ScriptApproval, type WireRequest } from "../src/index.ts";

test("session script decisions preserve the exact tuple and mutation retry key", async () => {
  const client = new ShadeClient({ socket: "/not/used", actor: { kind: "agent", id: "script-test" } });
  const approval: ScriptApproval = { provider: "npm", package: "@fixture/addon", version: "1.2.3", integrity: "sha512-exact+/=" };
  const opened: OpenedSessionPayload = {
    session: "session-opaque", workspace: "workspace-opaque", lease: "lease-opaque", cwd: "/fixture/workspace", env: {},
    compact_context: { workspace: "workspace-opaque", session: "session-opaque", base_ref: "main", base_sha: "oid", head_sha: "oid", changes: { staged: 0, unstaged: 0, untracked: 0 }, lease: "active", dependencies: { state: "ready" } },
  };
  const requests: WireRequest[] = [];
  let allowed = false;
  Reflect.set(client, "transport", {
    request: async (request: WireRequest) => {
      requests.push(request);
      let result: unknown;
      if (request.type === "execute" && request.intent.kind === "session_open") result = opened;
      else if (request.type === "execute" && request.intent.kind === "dependency_script_decision") {
        allowed = request.intent.allow;
        result = { allowed, refresh_required: true };
      } else if (request.type === "query" && request.query.kind === "dependency_scripts") {
        result = { scripts: [{ allowed, script: { approval, events: ["postinstall"], executed: false } }] };
      } else if (request.type === "execute" && request.intent.kind === "workspace_release") result = { released: true };
      else throw new Error("Unexpected request");
      return { v: 1, request_id: request.request_id, status: "ok", outcome: { state: "completed", result } };
    },
  });
  const session = await client.sessions.open({ session_id: opened.session, repository: { kind: "local", path: "/fixture" } });
  try {
    expect((await session.dependencyScripts()).scripts[0]?.allowed).toBe(false);
    expect(await session.approveScript(approval, { idempotency_key: "approve-exact" })).toEqual({ state: "completed", result: { allowed: true, refresh_required: true } });
    expect(requests.at(-1)).toMatchObject({ type: "execute", idempotency_key: "approve-exact", intent: { kind: "dependency_script_decision", selector: { workspace_id: opened.workspace, cwd: opened.cwd }, approval, allow: true } });
    expect((await session.dependencyScripts()).scripts[0]?.allowed).toBe(true);
    await session.revokeScript(approval, { idempotency_key: "revoke-exact" });
    expect(requests.at(-1)).toMatchObject({ type: "execute", idempotency_key: "revoke-exact", intent: { approval, allow: false } });
    expect((await session.dependencyScripts()).scripts[0]?.allowed).toBe(false);
  } finally { await session.release(); }
  await expect(session.approveScript(approval)).rejects.toThrow();
});
