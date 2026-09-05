import { describe, expect, test } from "bun:test";

import { ShadeClient, ShadeError, ShadeTimeoutError } from "../src/index.ts";

describe("structured errors", () => {
  test("preserves daemon recovery fields without parsing opaque ids", () => {
    const error = new ShadeError({
      code: "PUBLISH_CONFLICT",
      retry: "after_resolution",
      operation: "opaque:any/value",
      next: "session.context()",
      diagnostics_id: "diag/opaque",
    });

    expect(error.operation_id).toBe("opaque:any/value");
    expect(error.toJSON()).toEqual({
      code: "PUBLISH_CONFLICT",
      retry: "after_resolution",
      operation: "opaque:any/value",
      next: "session.context()",
      diagnostics_id: "diag/opaque",
    });
  });

  test("timeout after acceptance directs callers to operation polling", () => {
    const error = new ShadeTimeoutError("op-opaque", {
      idempotency_key: "publish:opaque",
    });
    expect(error.code).toBe("CLIENT_TIMEOUT");
    expect(error.retry).toBe("query_operation");
    expect(error.operation_id).toBe("op-opaque");
    expect(error.idempotency_key).toBe("publish:opaque");
    expect(error.next).toBe("operations.wait(operation_id)");
  });

  test("timeout before acceptance retains the private generated retry key", () => {
    const error = new ShadeTimeoutError(undefined, {
      idempotency_key: "generated:opaque",
    });
    expect(error.operation_id).toBeUndefined();
    expect(error.idempotency_key).toBe("generated:opaque");
    expect(error.retry).toBe("query_operation");
    expect(error.next).toBe("operations.waitByKey(idempotency_key)");
  });

  test("rejects an invalid explicit idempotency key before transport", async () => {
    const client = new ShadeClient({
      socket: "/not/used",
      actor: { kind: "host", id: "test" },
    });
    await expect(
      client.sessions.open(
        {
          session_id: "opaque-session",
          repository: { kind: "local", path: "/repo" },
        },
        { idempotency_key: "" },
      ),
    ).rejects.toMatchObject({
      code: "CLIENT_INVALID_IDEMPOTENCY_KEY",
      retry: "never",
    });
  });

  test("recovers the durable operation after the execute response times out", async () => {
    let generatedKey: string | undefined;
    let lookupAttempts = 0;
    const client = new ShadeClient({
      socket: "/not/used",
      actor: { kind: "host", id: "timeout-integration" },
      // The transport injects the execute timeout. Give recovery a real retry
      // window instead of relying on a 25 ms timer resuming within 30 ms.
      timeout_ms: 1_000,
      operation_poll_ms: 1,
    });
    Reflect.set(client, "transport", {
      request: async (request: any) => {
        if (request.type === "execute") {
          generatedKey = request.idempotency_key;
          throw new ShadeTimeoutError();
        }
        lookupAttempts += 1;
        expect(request.query).toEqual({
          kind: "operation_by_key",
          actor_kind: "host",
          actor_id: "timeout-integration",
          idempotency_key: generatedKey,
        });
        if (lookupAttempts === 1) {
          return {
            v: 1,
            request_id: request.request_id,
            status: "error",
            error: { code: "OPERATION_NOT_FOUND", retry: "never" },
          };
        }
        return {
          v: 1,
          request_id: request.request_id,
          status: "ok",
          outcome: {
            state: "completed",
            result: {
              id: "operation-after-timeout",
              state: "running",
              intent_kind: "session_open",
              outcome: null,
              error: null,
              created_at_ms: 1,
              updated_at_ms: 1,
            },
          },
        };
      },
    });

    let caught: unknown;
    try {
      await client.sessions.open({
        session_id: "timeout-session",
        repository: { kind: "local", path: "/repo" },
      });
    } catch (error) {
      caught = error;
    }

    expect(caught).toBeInstanceOf(ShadeTimeoutError);
    expect(caught).toMatchObject({
      operation_id: "operation-after-timeout",
      idempotency_key: generatedKey,
      next: "operations.wait(operation_id)",
    });
    expect(generatedKey).toBeString();
    expect(generatedKey?.length).toBeGreaterThan(0);
    expect(lookupAttempts).toBe(2);
  });

  test("retains the generated key when polling an accepted operation times out", async () => {
    let generatedKey: string | undefined;
    const client = new ShadeClient({
      socket: "/not/used",
      actor: { kind: "host", id: "accepted-timeout" },
      timeout_ms: 20,
      operation_poll_ms: 5,
    });
    Reflect.set(client, "transport", {
      request: async (request: any) => {
        if (request.type === "execute") {
          generatedKey = request.idempotency_key;
          return {
            v: 1,
            request_id: request.request_id,
            status: "ok",
            outcome: {
              state: "accepted",
              result: { operation_id: "accepted-operation" },
            },
          };
        }
        return {
          v: 1,
          request_id: request.request_id,
          status: "ok",
          outcome: {
            state: "completed",
            result: {
              id: "accepted-operation",
              state: "running",
              intent_kind: "session_open",
              outcome: null,
              error: null,
              created_at_ms: 1,
              updated_at_ms: 1,
            },
          },
        };
      },
    });

    let caught: unknown;
    try {
      await client.sessions.open({
        session_id: "accepted-timeout-session",
        repository: { kind: "local", path: "/repo" },
      });
    } catch (error) {
      caught = error;
    }

    expect(caught).toMatchObject({
      operation_id: "accepted-operation",
      idempotency_key: generatedKey,
    });
  });
});
